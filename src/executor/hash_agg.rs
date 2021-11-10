use super::*;
use crate::array::{ArrayBuilderImpl, ArrayImpl};
use crate::binder::{BoundAggCall, BoundExpr};
use crate::executor::aggregation::AggregationState;
use crate::types::DataValue;
use itertools::Itertools;
use smallvec::SmallVec;
use std::collections::HashMap;

/// The executor of hash aggregation.
pub struct HashAggExecutor {
    pub agg_calls: Vec<BoundAggCall>,
    pub group_keys: Vec<BoundExpr>,
    pub child: BoxedExecutor,
}

pub type HashKey = SmallVec<[DataValue; 16]>;
pub type HashValue = SmallVec<[Box<dyn AggregationState>; 16]>;

impl HashAggExecutor {
    fn execute_inner(
        state_entries: &mut HashMap<Arc<HashKey>, HashValue>,
        chunk: DataChunk,
        agg_calls: &[BoundAggCall],
        group_keys: &[BoundExpr],
    ) -> Result<(), ExecutorError> {
        // Eval group keys and arguments
        let group_cols: SmallVec<[ArrayImpl; 16]> = group_keys
            .iter()
            .map(|e| e.eval_array(&chunk))
            .try_collect()?;
        let arrays: SmallVec<[ArrayImpl; 16]> = agg_calls
            .iter()
            .map(|agg| agg.args[0].eval_array(&chunk))
            .try_collect()?;

        // Update states
        let num_rows = chunk.cardinality();
        for row_idx in 0..num_rows {
            let mut group_key = HashKey::new();
            for col in group_cols.iter() {
                group_key.push(col.get(row_idx));
            }
            let group_key = Arc::new(group_key);

            if !state_entries.contains_key(&group_key) {
                state_entries.insert(group_key.clone(), create_agg_states(agg_calls));
            }
            // since we just checked existence, the key must exist so we `unwrap` directly
            let states = state_entries.get_mut(&group_key).unwrap();
            for (array, state) in arrays.iter().zip(states.iter_mut()) {
                // TODO: support aggregations with multiple arguments
                state.update_single(&array.get(row_idx))?;
            }
        }

        Ok(())
    }

    fn finish_agg(
        state_entries: HashMap<Arc<HashKey>, HashValue>,
        agg_calls: Vec<BoundAggCall>,
        group_keys: Vec<BoundExpr>,
    ) -> DataChunk {
        let mut key_builders = group_keys
            .iter()
            .map(|e| ArrayBuilderImpl::new(e.return_type.as_ref().unwrap()))
            .collect::<Vec<ArrayBuilderImpl>>();
        let mut res_builders = agg_calls
            .iter()
            .map(|agg| ArrayBuilderImpl::new(&agg.return_type))
            .collect::<Vec<ArrayBuilderImpl>>();
        for (key, val) in state_entries.iter() {
            // Push group key
            for (k, builder) in key.iter().zip(key_builders.iter_mut()) {
                builder.push(k);
            }
            // Push aggregate result
            for (state, builder) in val.iter().zip(res_builders.iter_mut()) {
                builder.push(&state.output());
            }
        }
        key_builders.append(&mut res_builders);
        key_builders
            .into_iter()
            .map(|builder| builder.finish())
            .collect::<DataChunk>()
    }

    pub fn execute(self) -> impl Stream<Item = Result<DataChunk, ExecutorError>> {
        try_stream! {
            let mut state_entries = HashMap::new();

            for await chunk in self.child {
                let chunk = chunk?;
                Self::execute_inner(&mut state_entries, chunk, &self.agg_calls, &self.group_keys)?;
            }

            let chunk = Self::finish_agg(state_entries, self.agg_calls, self.group_keys);
            yield chunk;
        }
    }
}
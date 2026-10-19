use super::*;
use crate::jit::entry_bytes::Hint;

impl<'gc> RawTable<'gc> {
    /// Reads a string using its verified canonical hash and a checked bucket hint.
    pub(crate) fn get_code_string(
        &self,
        mc: &Mutation<'gc>,
        key: String<'gc>,
        canonical_hash: PreparedStringHash,
        hint: Option<Hint<'_>>,
    ) -> Value<'gc> {
        let raw = self.map.raw_table();
        if let Some(index) = hint.and_then(Hint::load) {
            if index < raw.buckets() && unsafe { raw.is_bucket_full(index) } {
                // The live bucket belongs to the borrowed current table.
                let bucket = unsafe { raw.bucket(index).as_ref() };
                if bucket.0.eq_string(key) {
                    return bucket.1 .0.get(mc);
                }
            }
        }
        if let Some(bucket) = raw.find(canonical_hash.bits(), |(stored, _)| stored.eq_string(key)) {
            // The index is derived from this table's successful canonical lookup.
            let index = unsafe { raw.bucket_index(&bucket) };
            if let Some(hint) = hint {
                hint.remember(Some(index));
            }
            // The found bucket remains live under the current immutable borrow.
            unsafe { bucket.as_ref().1 .0.get(mc) }
        } else {
            if let Some(hint) = hint {
                hint.remember(None);
            }
            Value::Nil
        }
    }
}

use crate::{Store, Op, Error};
use crate::model::Version;
impl Store {
    pub fn apply(&mut self, expected: Option<u64>, operations: &[Op]) -> Result<u64, Error> {
        self.revision += 1;
        for operation in operations {
            let (key, value) = match operation {
                Op::Put(key, value) => (key, Some(value.clone())),
                Op::Delete(key) => (key, None),
            };
            self.history.entry(key.clone()).or_default().push(Version { revision: self.revision, value });
        }
        if expected.is_some_and(|revision| revision != self.revision - 1) { return Err(Error::Conflict); }
        Ok(self.revision)
    }
}

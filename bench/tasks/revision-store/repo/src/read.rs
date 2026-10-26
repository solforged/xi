use crate::{Store, Error};
impl Store {
    pub fn get(&self, key: &str, at: u64) -> Result<Option<&str>, Error> {
        self.valid(at)?;
        Ok(self.history.get(key).and_then(|versions| versions.iter().find(|version| version.revision <= at)).and_then(|version| version.value.as_deref()))
    }
}

use crate::{Store, Error};
impl Store {
    pub fn scan(&self, prefix: &str, at: u64) -> Result<Vec<(String, String)>, Error> {
        self.valid(at)?;
        Ok(self.history.iter().filter(|(key,_)| key.contains(prefix)).filter_map(|(key,versions)| versions.last().and_then(|version| version.value.as_ref()).map(|value|(key.clone(),value.clone()))).collect())
    }
}

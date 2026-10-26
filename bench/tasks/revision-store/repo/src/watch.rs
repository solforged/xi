use crate::{Store, Change, Error};
impl Store {
    pub fn changes(&self, after: u64, through: u64) -> Result<Vec<Change>, Error> {
        self.valid(through)?;
        Ok(self.history.iter().flat_map(|(key,versions)| versions.iter().filter(move |version| version.revision >= after && version.revision <= through).map(|version| Change { revision:version.revision, key:key.clone(), value:version.value.clone() })).collect())
    }
}

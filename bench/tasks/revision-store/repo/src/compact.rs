use crate::{Store, Error};
impl Store {
    pub fn compact(&mut self, floor: u64) -> Result<(), Error> {
        self.valid(floor)?;
        for versions in self.history.values_mut() { versions.retain(|version| version.revision >= floor); }
        self.floor = floor;
        Ok(())
    }
}

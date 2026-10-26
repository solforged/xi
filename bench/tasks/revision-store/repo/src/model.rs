use std::collections::BTreeMap;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error { Conflict, InvalidRevision, DuplicateKey }
#[derive(Debug, Clone)]
pub enum Op { Put(String, String), Delete(String) }
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change { pub revision: u64, pub key: String, pub value: Option<String> }
#[derive(Debug, Clone)]
pub(crate) struct Version { pub revision: u64, pub value: Option<String> }
#[derive(Debug, Default)]
pub struct Store {
    pub(crate) revision: u64,
    pub(crate) floor: u64,
    pub(crate) history: BTreeMap<String, Vec<Version>>,
}
impl Store {
    pub fn new() -> Self { Self::default() }
    pub fn revision(&self) -> u64 { self.revision }
    pub fn floor(&self) -> u64 { self.floor }
    pub fn retained_versions(&self) -> usize { self.history.values().map(Vec::len).sum() }
    pub(crate) fn valid(&self, at: u64) -> Result<(), Error> {
        if at > self.revision { Err(Error::InvalidRevision) } else { Ok(()) }
    }
}

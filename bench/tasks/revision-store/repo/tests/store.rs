use revision_store::{Store, Op, Error, Change};
fn put(key: &str, value: &str) -> Op { Op::Put(key.into(), value.into()) }
fn del(key: &str) -> Op { Op::Delete(key.into()) }
#[test]
fn transactions_are_atomic_and_empty_does_not_tick() {
    let mut s=Store::new();
    assert_eq!(s.apply(Some(0), &[]), Ok(0));
    assert_eq!(s.apply(Some(0), &[put("a","1"),put("b","2")]), Ok(1));
    assert_eq!(s.apply(Some(0), &[put("a","bad")]), Err(Error::Conflict));
    assert_eq!(s.get("a",1),Ok(Some("1")));
    assert_eq!(s.apply(Some(1), &[put("x","1"),del("x")]),Err(Error::DuplicateKey));
    assert_eq!(s.apply(Some(0), &[del("a"),del("a")]),Err(Error::Conflict));
    assert_eq!(s.apply(Some(0), &[]),Err(Error::Conflict));
    assert_eq!(s.revision(),1);
    assert_eq!(s.retained_versions(),2);
    assert_eq!(s.get("x",1),Ok(None));
}
#[test]
fn snapshots_and_prefixes_use_latest_visible_version() {
    let mut s=Store::new();
    s.apply(None,&[put("ab","old"),put("zab","not-prefix"),put("a","one")]).unwrap();
    s.apply(None,&[put("ab","new"),del("a")]).unwrap();
    s.apply(None,&[put("a","three")]).unwrap();
    assert_eq!(s.get("ab",1),Ok(Some("old")));
    assert_eq!(s.get("ab",2),Ok(Some("new")));
    assert_eq!(s.get("a",2),Ok(None));
    assert_eq!(s.scan("a",1).unwrap(),vec![("a".into(),"one".into()),("ab".into(),"old".into())]);
    assert_eq!(s.scan("a",2).unwrap(),vec![("ab".into(),"new".into())]);
    assert!(s.scan("",0).unwrap().is_empty());
    assert_eq!(s.get("missing",4),Err(Error::InvalidRevision));
}
#[test]
fn watch_is_exclusive_ordered_and_keeps_tombstones() {
    let mut s=Store::new();
    s.apply(None,&[put("z","1"),put("a","1")]).unwrap();
    s.apply(None,&[put("z","1"),del("missing")]).unwrap();
    let changes=s.changes(0,2).unwrap();
    assert_eq!(changes,vec![Change{revision:1,key:"a".into(),value:Some("1".into())},Change{revision:1,key:"z".into(),value:Some("1".into())},Change{revision:2,key:"missing".into(),value:None},Change{revision:2,key:"z".into(),value:Some("1".into())}]);
    assert_eq!(s.changes(1,2).unwrap().len(),2);
    assert!(s.changes(2,2).unwrap().is_empty());
    assert_eq!(s.changes(2,1),Err(Error::InvalidRevision));
    assert_eq!(s.changes(3,3),Err(Error::InvalidRevision));
}
#[test]
fn compaction_preserves_baselines_and_rejects_expired_reads() {
    let mut s=Store::new();
    s.apply(None,&[put("a","1"),put("b","1")]).unwrap();
    s.apply(None,&[put("a","2"),del("b")]).unwrap();
    s.apply(None,&[put("c","3")]).unwrap();
    s.apply(None,&[put("a","4"),put("b","4")]).unwrap();
    let snapshots:Vec<_>=(2..=4).map(|at|s.scan("",at).unwrap()).collect();
    let watch=s.changes(2,4).unwrap();
    s.compact(2).unwrap();
    assert_eq!(s.retained_versions(),5);
    for at in 2..=4 { assert_eq!(s.scan("",at).unwrap(),snapshots[(at-2) as usize]); }
    assert_eq!(s.changes(2,4).unwrap(),watch);
    assert_eq!(s.get("absent",1),Err(Error::InvalidRevision));
    assert_eq!(s.scan("no-match",1),Err(Error::InvalidRevision));
    assert_eq!(s.changes(1,4),Err(Error::InvalidRevision));
    s.compact(2).unwrap();
    assert_eq!(s.retained_versions(),5);
    assert_eq!(s.compact(1),Err(Error::InvalidRevision));
    assert_eq!(s.compact(5),Err(Error::InvalidRevision));
    assert_eq!((s.floor(),s.revision(),s.retained_versions()),(2,4,5));
    s.compact(4).unwrap();
    assert_eq!(s.retained_versions(),3);
    assert_eq!(s.scan("",4).unwrap(),snapshots[2]);
}
#[test]
fn deterministic_history_matches_a_snapshot_oracle() {
    use std::collections::BTreeMap;
    let mut s=Store::new();
    let mut state=BTreeMap::<String,String>::new();
    let mut snapshots=vec![state.clone()];
    for revision in 1..=80u64 {
        let key=format!("k{}",(revision*7)%13);
        let op=if revision%4==0 { state.remove(&key); Op::Delete(key) } else { let value=format!("v{revision}"); state.insert(key.clone(),value.clone()); Op::Put(key,value) };
        assert_eq!(s.apply(Some(revision-1),&[op]),Ok(revision));
        snapshots.push(state.clone());
    }
    for floor in [0,1,7,19,44,80] {
        s.compact(floor).unwrap();
        for at in floor..=80 {
            assert_eq!(s.scan("",at).unwrap(),snapshots[at as usize].iter().map(|(k,v)|(k.clone(),v.clone())).collect::<Vec<_>>());
            for index in 0..14 { let key=format!("k{index}"); assert_eq!(s.get(&key,at).unwrap(),snapshots[at as usize].get(&key).map(String::as_str)); }
        }
    }
}

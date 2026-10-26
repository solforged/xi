# Revisioned store contract

Repair the existing public API across modules. No dependencies. A store begins at revision 0 with floor 0. Each successful nonempty apply creates exactly one revision for the entire transaction, even if values are unchanged or deletes target missing keys. An empty apply does not advance the revision. If expected is Some, it must equal the current revision, including for empty transactions. Check conflict before duplicate keys. A repeated key in one transaction is DuplicateKey, regardless of operation kind. All failed applies are atomic: no revision, floor, history or visible state changes.

get and scan take a historical revision. Read the latest version at or before that revision, with None tombstones hiding older values. scan matches starts_with (not substring), is lexicographic by key, and omits tombstones. Revision bounds for all reads are inclusive floor..=revision; invalid revisions are errors even for missing keys or empty results.

changes(after, through) returns every change with after < revision <= through, sorted by (revision, key), including deletes of missing keys and repeated unchanged values. Both bounds must be valid and after <= through. An empty valid interval is empty.

compact(new_floor) accepts only floor <= new_floor <= revision. Keep exactly the newest version at or before new_floor for each key, plus every newer version. Keep baseline tombstones too, and do not synthesize a baseline for keys created later. Retained version counts are observable. Compaction never changes revision, snapshot answers in the surviving interval, or changes after the floor. Invalid compaction is atomic. Repeating the same compaction is idempotent. Do not change public signatures.

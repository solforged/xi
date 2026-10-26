from registry import select_policy


def export_ids(documents, mode):
    predicate = select_policy(mode)
    return [doc["id"] for doc in documents if predicate(doc)]

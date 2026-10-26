def include(document):
    return bool(document["tags"]) and not document["deleted"]

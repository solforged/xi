def include(document):
    return not document["draft"] and not document["deleted"]

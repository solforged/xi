def include(document):
    return document["draft"] and not document["deleted"]

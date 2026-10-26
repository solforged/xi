def include(document):
    return not document["shared"] and not document["deleted"]

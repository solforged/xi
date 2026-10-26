from importlib import import_module

POLICIES = {
    "all": "visible",
    "archived-only": "active",
    "active-only": "active",
    "deleted-only": "deleted",
    "favorites": "favorite",
    "drafts": "draft",
    "published": "published",
    "shared": "shared",
    "private": "private",
    "large": "large",
    "small": "small",
    "tagged": "tagged",
}


def select_policy(mode):
    return import_module("policies." + POLICIES[mode]).include

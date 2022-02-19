#!/usr/bin/python3

# This script is used to verify that all combinations of enabled / disabled features
# compile, produce no warnings, and pass tests.

import subprocess as sp


def powerset(elements):
    if not elements:
        return [[]]
    return powerset(elements[1:]) + [[elements[0]] + x for x in powerset(elements[1:])]


FEATURES = [
    "client",
    "derive",
    "server",
    "axum-server",
    "i8",
    "nil",
    "dxr_derive",
    "reqwest",
    "url",
    "async-trait",
    "axum",
    "http",
    "tokio",
]


def main():
    allcombos = powerset(FEATURES)
    allcombos.remove([])

    features = [["--all-features"], ["--no-default-features"]]
    features += [["--no-default-features", "--features", ",".join(features)] for features in allcombos]

    for featureset in features:
        for command in ["check", "clippy", "build", "test"]:
        # for command in ["check", "clippy"]:
            print(f">> cargo {command}", " ".join(featureset))

            # cargo test --all-targets skips doctests
            targets = ["--all-targets"] if command != "test" else []
            ret = sp.run(["cargo", command] + targets + featureset)

            try:
                ret.check_returncode()
            except sp.CalledProcessError:
                break


if __name__ == "__main__":
    main()

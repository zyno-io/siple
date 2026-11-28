#!/usr/bin/env python3
"""Strict cross-check of the generated Flatpak cargo sources against Cargo.lock.

    flatpak-cargo-check.py LOCK SOURCES.json    exit 1 with a reason when anything is off
    flatpak-cargo-check.py --self-test          negative cases (must all fail) and a positive one

The generated file is executed by the build (archives are unpacked into the vendor directory and the
cargo config decides where cargo reads crates from), so it must hold exactly this, nothing else:
  - per registry package of the lock: one `archive` (the lock's crates.io URL and sha256, dest
    cargo/vendor/<name>-<version>) and one `inline` `.cargo-checksum.json` at the same dest whose
    contents are exactly {"package": <lock checksum>, "files": {}};
  - per git package of the lock (only those in GIT_ALLOWED, pinned with `rev`): one `git` source per
    repo commit (dest flatpak-cargo/git/<repo>-<commit[:7]>), one `shell` copying exactly the allowed
    path of that checkout to cargo/vendor/<name>, an `inline` `Cargo.toml` there whose package name and
    version match the lock, and an `inline` `.cargo-checksum.json` there that is exactly
    {"package": null, "files": {}};
  - one `inline` `config` in `cargo` with the vendoring config below, byte for byte (with git
    packages: the same tables plus one `[source."<repo>"]` per repo with its `rev`, compared parsed).
Needs Python >= 3.11 (tomllib). Run with -I.
"""
import json
import re
import sys
import tomllib

REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
URL_RX = re.compile(r"^https://static\.crates\.io/crates/([^/]+)/\1-(.+)\.crate$")
CONFIG_CONTENTS = (
    '[source.vendored-sources]\ndirectory = "cargo/vendor"\n\n'
    "[source.crates-io]\nreplace-with = \"vendored-sources\"\n"
)
ARCHIVE_KEYS = {"type", "archive-type", "url", "sha256", "dest"}
INLINE_KEYS = {"type", "contents", "dest", "dest-filename"}
GIT_KEYS = {"type", "url", "commit", "dest"}
SHELL_KEYS = {"type", "commands"}
# The only git dependencies allowed: repo URL (Cargo canonical form) -> {package name: path in the repo}.
# winit: fork without the private CGS blur API (Mac App Store); see [patch.crates-io] in Cargo.toml.
GIT_ALLOWED = {"https://github.com/zynoconsulting/winit": {"winit": "."}}
GIT_SOURCE_RX = re.compile(r"^git\+(https://[^?#]+)\?rev=([0-9a-f]{40})#\2$")


class Bad(Exception):
    pass


def lock_packages(lock):
    """Returns ({(name, version): checksum} of registry packages, {name: (repo, commit, version)} of git ones)."""
    want, git = {}, {}
    for p in lock["package"]:
        src = p.get("source")
        if src is None:
            continue  # workspace or path crate
        label = f"{p['name']} {p['version']}"
        if src.startswith("git+"):
            m = GIT_SOURCE_RX.fullmatch(src)
            if not m:
                raise Bad(f"{label} is a git dependency not pinned with rev=<40-hex commit>: {src}")
            repo, commit = m.groups()
            if p["name"] not in GIT_ALLOWED.get(repo, {}):
                raise Bad(f"{label} is a git dependency from {repo}, which GIT_ALLOWED does not allow")
            if p["name"] in git:
                raise Bad(f"{label}: more than one git package named {p['name']}")
            git[p["name"]] = (repo, commit, p["version"])
            continue
        if src != REGISTRY:
            raise Bad(f"{label} comes from an unknown registry: {src}")
        chk = p.get("checksum")
        if not isinstance(chk, str) or not re.fullmatch(r"[0-9a-f]{64}", chk):
            raise Bad(f"{label} has no valid checksum in the lock file")
        want[(p["name"], p["version"])] = chk
    return want, git


def git_checkout(repo, commit):
    return f"flatpak-cargo/git/{repo.rsplit('/', 1)[-1]}-{commit[:7]}"


def expected_config(git):
    src = {"vendored-sources": {"directory": "cargo/vendor"}, "crates-io": {"replace-with": "vendored-sources"}}
    for repo, commit, _ in git.values():
        src[repo] = {"git": repo, "replace-with": "vendored-sources", "rev": commit}
    return {"source": src}


def check(lock, sources):
    """Raises Bad unless `sources` is exactly what the lock file calls for."""
    want, git = lock_packages(lock)
    if not isinstance(sources, list):
        raise Bad("the sources are not a JSON list")
    by_dest = {f"cargo/vendor/{n}-{v}": (n, v) for n, v in want}
    git_by_dest = {f"cargo/vendor/{n}": n for n in git}
    want_checkouts = {(r, c): git_checkout(r, c) for r, c, _ in git.values()}
    want_copies = {
        n: f'cp -r --reflink=auto "{git_checkout(r, c)}/{GIT_ALLOWED[r][n]}" "cargo/vendor/{n}"'
        for n, (r, c, _) in git.items()
    }
    archives, sums, configs = {}, {}, []
    checkouts, copies, git_tomls, git_sums = set(), set(), set(), set()
    for s in sources:
        if not isinstance(s, dict):
            raise Bad(f"entry is not an object: {s!r}")
        t = s.get("type")
        if t == "archive":
            if set(s) != ARCHIVE_KEYS:
                raise Bad(f"archive with unexpected keys {sorted(s)}: {s.get('url')}")
            m = URL_RX.fullmatch(s["url"]) if isinstance(s["url"], str) else None
            if not m:
                raise Bad(f"unexpected archive url {s['url']!r}")
            key = (m.group(1), m.group(2))
            if key not in want:
                raise Bad(f"archive for {key} which is not in the lock file")
            if key in archives:
                raise Bad(f"duplicate archive for {key}")
            if s["archive-type"] != "tar-gzip":
                raise Bad(f"archive {key}: archive-type {s['archive-type']!r}")
            if s["sha256"] != want[key]:
                raise Bad(f"archive {key}: sha256 differs from the lock file")
            if s["dest"] != f"cargo/vendor/{key[0]}-{key[1]}":
                raise Bad(f"archive {key}: dest {s['dest']!r}")
            archives[key] = True
        elif t == "inline":
            if set(s) != INLINE_KEYS:
                raise Bad(f"inline with unexpected keys {sorted(s)}")
            name, dest, body = s["dest-filename"], s["dest"], s["contents"]
            if name == "config":
                if dest != "cargo":
                    raise Bad(f"cargo config at {dest!r}")
                if not git:
                    if body != CONFIG_CONTENTS:
                        raise Bad("the cargo config differs from the expected text")
                else:
                    try:
                        parsed = tomllib.loads(body)
                    except (TypeError, tomllib.TOMLDecodeError):
                        raise Bad("the cargo config is not TOML")
                    if parsed != expected_config(git):
                        raise Bad("the cargo config differs from the expected tables")
                configs.append(s)
            elif isinstance(dest, str) and dest in git_by_dest:
                pkg = git_by_dest[dest]
                if name == "Cargo.toml":
                    if pkg in git_tomls:
                        raise Bad(f"duplicate Cargo.toml for git package {pkg}")
                    try:
                        manifest = tomllib.loads(body)
                    except (TypeError, tomllib.TOMLDecodeError):
                        raise Bad(f"Cargo.toml of git package {pkg} is not TOML")
                    p = manifest.get("package", {})
                    if p.get("name") != pkg or p.get("version") != git[pkg][2]:
                        raise Bad(f"Cargo.toml of git package {pkg} names {p.get('name')} {p.get('version')}")
                    git_tomls.add(pkg)
                elif name == ".cargo-checksum.json":
                    if pkg in git_sums:
                        raise Bad(f"duplicate checksum file for git package {pkg}")
                    try:
                        parsed = json.loads(body)
                    except (TypeError, ValueError):
                        raise Bad(f"checksum file of git package {pkg} is not JSON")
                    if parsed != {"package": None, "files": {}}:
                        raise Bad(f"checksum file of git package {pkg} is not exactly {{package: null, files: {{}}}}")
                    git_sums.add(pkg)
                else:
                    raise Bad(f"unexpected inline file {name!r} in {dest!r}")
            elif name == ".cargo-checksum.json":
                key = by_dest.get(dest) if isinstance(dest, str) else None
                if key is None:
                    raise Bad(f"checksum file at {dest!r} which is no crate of the lock file")
                if key in sums:
                    raise Bad(f"duplicate checksum file for {key}")
                try:
                    parsed = json.loads(body)
                except (TypeError, ValueError):
                    raise Bad(f"checksum file of {key} is not JSON")
                if parsed != {"package": want[key], "files": {}}:
                    raise Bad(f"checksum file of {key} does not hold exactly the lock's checksum and no files")
                sums[key] = True
            else:
                raise Bad(f"unexpected inline file {name!r} in {dest!r}")
        elif t == "git":
            if set(s) != GIT_KEYS:
                raise Bad(f"git source with unexpected keys {sorted(s)}")
            key = (s["url"], s["commit"])
            if key not in want_checkouts:
                raise Bad(f"git source {key} which is not in the lock file")
            if s["dest"] != want_checkouts[key]:
                raise Bad(f"git source {key}: dest {s['dest']!r}")
            if key in checkouts:
                raise Bad(f"duplicate git source {key}")
            checkouts.add(key)
        elif t == "shell":
            if set(s) != SHELL_KEYS:
                raise Bad(f"shell with unexpected keys {sorted(s)}")
            cmds = s["commands"]
            pkg = next((n for n, c in want_copies.items() if cmds == [c]), None)
            if pkg is None:
                raise Bad(f"unexpected shell commands {cmds!r}")
            if pkg in copies:
                raise Bad(f"duplicate copy of git package {pkg}")
            copies.add(pkg)
        else:
            raise Bad(f"unknown source type {t!r} in the generated sources")
    if len(configs) != 1:
        raise Bad(f"expected exactly one cargo config, found {len(configs)}")
    for what, got in (("archive", archives), ("checksum file", sums)):
        if set(got) != set(want):
            missing = sorted(set(want) - set(got))[:5]
            raise Bad(f"missing {what}s: {missing}")
    if checkouts != set(want_checkouts):
        raise Bad(f"missing git sources: {sorted(set(want_checkouts) - checkouts)}")
    for what, got in (("copy", copies), ("Cargo.toml", git_tomls), ("checksum file", git_sums)):
        if got != set(git):
            raise Bad(f"missing git package {what}s: {sorted(set(git) - got)}")
    return len(want) + len(git)


def self_test():
    import copy

    sha = lambda c: c * 64
    lock = {
        "package": [
            {"name": "siple", "version": "0.1.0"},
            {"name": "a", "version": "1.0.0", "source": REGISTRY, "checksum": sha("a")},
            {"name": "b-c", "version": "2.1.0-rc.1", "source": REGISTRY, "checksum": sha("b")},
        ]
    }

    def good():
        out = []
        for n, v, c in (("a", "1.0.0", "a"), ("b-c", "2.1.0-rc.1", "b")):
            dest = f"cargo/vendor/{n}-{v}"
            out.append({"type": "archive", "archive-type": "tar-gzip",
                        "url": f"https://static.crates.io/crates/{n}/{n}-{v}.crate",
                        "sha256": sha(c), "dest": dest})
            out.append({"type": "inline", "contents": json.dumps({"package": sha(c), "files": {}}),
                        "dest": dest, "dest-filename": ".cargo-checksum.json"})
        out.append({"type": "inline", "contents": CONFIG_CONTENTS, "dest": "cargo", "dest-filename": "config"})
        return out

    check(lock, good())  # the positive case

    def mutate(f):
        s = good()
        f(s)
        return s

    cfg = lambda s: s[-1]
    cases = {
        "extra inline file in a crate dir": lambda s: s.append(
            {"type": "inline", "contents": "x", "dest": "cargo/vendor/a-1.0.0", "dest-filename": "build.rs"}),
        "swapped archive dests": lambda s: (
            s[0].__setitem__("dest", "cargo/vendor/b-c-2.1.0-rc.1"),
            s[2].__setitem__("dest", "cargo/vendor/a-1.0.0")),
        "extra [build] rustflags in the cargo config": lambda s: cfg(s).__setitem__(
            "contents", CONFIG_CONTENTS + '\n[build]\nrustflags = ["-C", "link-arg=-evil"]\n'),
        "cargo config in the wrong dest": lambda s: cfg(s).__setitem__("dest", "cargo/vendor"),
        "second cargo config": lambda s: s.append(copy.deepcopy(cfg(s))),
        "no cargo config": lambda s: s.pop(),
        "wrong sha256": lambda s: s[0].__setitem__("sha256", sha("f")),
        "wrong archive url host": lambda s: s[0].__setitem__("url", "https://evil.example/crates/a/a-1.0.0.crate"),
        "duplicate archive": lambda s: s.append(copy.deepcopy(s[0])),
        "unknown crate archive": lambda s: s.append(
            {**copy.deepcopy(s[0]), "url": "https://static.crates.io/crates/z/z-9.9.9.crate",
             "dest": "cargo/vendor/z-9.9.9"}),
        "missing archive": lambda s: s.pop(2),
        "missing checksum file": lambda s: s.pop(1),
        "duplicate checksum file": lambda s: s.append(copy.deepcopy(s[1])),
        "checksum lists files": lambda s: s[1].__setitem__(
            "contents", json.dumps({"package": sha("a"), "files": {"x": sha("0")}})),
        "checksum of another package": lambda s: s[1].__setitem__(
            "contents", json.dumps({"package": sha("b"), "files": {}})),
        "checksum with an extra key": lambda s: s[1].__setitem__(
            "contents", json.dumps({"package": sha("a"), "files": {}, "x": 1})),
        "checksum contents not JSON": lambda s: s[1].__setitem__("contents", "nope"),
        "checksum file in the wrong dest": lambda s: s[1].__setitem__("dest", "cargo/vendor/b-c-2.1.0-rc.1"),
        "unknown key on an archive": lambda s: s[0].__setitem__("strip-components", 0),
        "unknown key on an inline": lambda s: s[1].__setitem__("mode", "0755"),
        "unknown source type": lambda s: s.append({"type": "shell", "commands": ["id"]}),
        "git source type": lambda s: s.append({"type": "git", "url": "https://x/y.git", "commit": "0"}),
        "file source type": lambda s: s.append({"type": "file", "url": "https://x/y", "sha256": sha("0")}),
    }
    failed = []
    for name, f in cases.items():
        try:
            check(lock, mutate(f))
        except Bad:
            continue
        failed.append(name)
    for name, bad_lock in {
        "git dependency": {"package": [{"name": "g", "version": "1.0.0", "source": "git+https://x/y#abc"}]},
        "foreign registry": {"package": [{"name": "g", "version": "1.0.0", "source": "registry+https://x/index", "checksum": sha("a")}]},
    }.items():
        try:
            check(bad_lock, [])
        except Bad:
            continue
        failed.append(name)
    # Git packages: the allowed winit repo, pinned by rev.
    repo, commit = "https://github.com/zynoconsulting/winit", "c" * 40
    git_src = f"git+{repo}?rev={commit}#{commit}"
    glock = copy.deepcopy(lock)
    glock["package"].append({"name": "winit", "version": "0.30.13", "source": git_src})
    checkout = f"flatpak-cargo/git/winit-{commit[:7]}"
    copy_cmd = f'cp -r --reflink=auto "{checkout}/." "cargo/vendor/winit"'
    gcfg = CONFIG_CONTENTS + f'\n[source."{repo}"]\ngit = "{repo}"\nreplace-with = "vendored-sources"\nrev = "{commit}"\n'

    def ggood():
        out = good()[:-1]
        out.insert(0, {"type": "git", "url": repo, "commit": commit, "dest": checkout})
        out += [
            {"type": "shell", "commands": [copy_cmd]},
            {"type": "inline", "contents": '[package]\nname = "winit"\nversion = "0.30.13"\n',
             "dest": "cargo/vendor/winit", "dest-filename": "Cargo.toml"},
            {"type": "inline", "contents": json.dumps({"package": None, "files": {}}),
             "dest": "cargo/vendor/winit", "dest-filename": ".cargo-checksum.json"},
            {"type": "inline", "contents": gcfg, "dest": "cargo", "dest-filename": "config"},
        ]
        return out

    check(glock, ggood())  # the positive git case
    by = lambda s, pred: next(e for e in s if pred(e))
    gsrc = lambda s: by(s, lambda e: e["type"] == "git")
    gsh = lambda s: by(s, lambda e: e["type"] == "shell")
    gtoml = lambda s: by(s, lambda e: e.get("dest-filename") == "Cargo.toml")
    gsum = lambda s: by(s, lambda e: e.get("dest") == "cargo/vendor/winit" and e.get("dest-filename") == ".cargo-checksum.json")
    gconf = lambda s: by(s, lambda e: e.get("dest-filename") == "config")
    gcases = {
        "git: no git source": lambda s: s.remove(gsrc(s)),
        "git: duplicate git source": lambda s: s.append(copy.deepcopy(gsrc(s))),
        "git: other commit": lambda s: gsrc(s).__setitem__("commit", "d" * 40),
        "git: other url": lambda s: gsrc(s).__setitem__("url", "https://github.com/evil/winit"),
        "git: wrong dest": lambda s: gsrc(s).__setitem__("dest", "cargo/vendor/winit"),
        "git: extra key": lambda s: gsrc(s).__setitem__("tag", "v1"),
        "git: no copy": lambda s: s.remove(gsh(s)),
        "git: duplicate copy": lambda s: s.append(copy.deepcopy(gsh(s))),
        "git: extra shell command": lambda s: gsh(s)["commands"].append("id"),
        "git: copy of another path": lambda s: gsh(s).__setitem__(
            "commands", [copy_cmd.replace(f"{checkout}/.", f"{checkout}/evil")]),
        "git: no Cargo.toml": lambda s: s.remove(gtoml(s)),
        "git: Cargo.toml of another package": lambda s: gtoml(s).__setitem__(
            "contents", '[package]\nname = "evil"\nversion = "0.30.13"\n'),
        "git: Cargo.toml of another version": lambda s: gtoml(s).__setitem__(
            "contents", '[package]\nname = "winit"\nversion = "9.9.9"\n'),
        "git: Cargo.toml not TOML": lambda s: gtoml(s).__setitem__("contents", "[[["),
        "git: no checksum file": lambda s: s.remove(gsum(s)),
        "git: checksum with a package hash": lambda s: gsum(s).__setitem__(
            "contents", json.dumps({"package": sha("a"), "files": {}})),
        "git: extra inline file": lambda s: s.append(
            {"type": "inline", "contents": "x", "dest": "cargo/vendor/winit", "dest-filename": "build.rs"}),
        "git: config without the git table": lambda s: gconf(s).__setitem__("contents", CONFIG_CONTENTS),
        "git: config with another rev": lambda s: gconf(s).__setitem__("contents", gcfg.replace(commit, "d" * 40)),
        "git: config with rustflags": lambda s: gconf(s).__setitem__(
            "contents", gcfg + '\n[build]\nrustflags = ["-C", "link-arg=-evil"]\n'),
    }
    for name, f in gcases.items():
        s = ggood()
        f(s)
        try:
            check(glock, s)
        except Bad:
            continue
        failed.append(name)
    pkg = lambda src, name="winit": {"package": [{"name": name, "version": "0.30.13", "source": src}]}
    glocks = {
        "git: repo not allowed": pkg(f"git+https://github.com/evil/winit?rev={commit}#{commit}"),
        "git: package not allowed": pkg(git_src, "dpi"),
        "git: branch instead of rev": pkg(f"git+{repo}?branch=main#{commit}"),
        "git: rev and commit differ": pkg(f"git+{repo}?rev={commit}#{'d' * 40}"),
        "git: short rev": pkg(f"git+{repo}?rev=ccccccc#ccccccc"),
    }
    for name, bad_lock in glocks.items():
        try:
            check(bad_lock, [])
        except Bad:
            continue
        failed.append(name)
    if failed:
        sys.exit("self-test FAILED, accepted: " + "; ".join(failed))
    n_bad = len(cases) + 2 + len(gcases) + len(glocks)
    print(f"self-test ok: 2 valid inputs accepted, {n_bad} tampered inputs rejected")


def main(argv):
    if argv == ["--self-test"]:
        self_test()
        return
    if len(argv) != 2:
        sys.exit("usage: flatpak-cargo-check.py LOCK SOURCES.json | --self-test")
    with open(argv[0], "rb") as f:
        lock = tomllib.load(f)
    with open(argv[1]) as f:
        sources = json.load(f)
    try:
        n = check(lock, sources)
    except Bad as e:
        sys.exit(f"error: generated sources rejected: {e}")
    print(f"verified {n} crates (archives, checksum files and cargo config) against {argv[0]}")


if __name__ == "__main__":
    main(sys.argv[1:])

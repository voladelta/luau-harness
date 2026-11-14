use super::approval::{Kind, Location, Lockfile, Snapshot, digest, snapshot};
use super::frontmatter_description as d;
use super::*;
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

/// A fresh, empty workspace under the temp dir.
fn ws(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hx-modules-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn write(path: PathBuf, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// `.harness/pkgs/<name>` with an fs=["read"] capability.
fn install_pkg(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(".harness/pkgs").join(name);
    write(
        dir.join("hx.toml"),
        "[package]\nversion = \"1.0\"\n\n[capabilities]\nfs = [\"read\"]\n",
    );
    write(dir.join("init.luau"), "return { x = 1 }\n");
    dir
}

/// The registry a run would capture once the owner trusts the workspace config as it is now.
fn trusted_registry(root: &Path) -> Registry {
    Registry::build(
        root,
        Some(crate::trust::Config::read(root).unwrap()),
        &crate::dirs::TestDirs::new(),
    )
    .unwrap()
}

/// The lock's verdict on a package directory.
fn verdict(root: &Path, name: &str, dir: &Path) -> Result<Snapshot> {
    Lockfile::read(&root.join("hx.lock"))
        .unwrap()
        .approved(name, Location::Package(dir.to_path_buf()))
}

fn err(r: Result<impl Sized>) -> String {
    match r {
        Ok(_) => panic!("expected an error"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn frontmatter_description_forms() {
    assert_eq!(
        d("---\nname: a\ndescription: plain words\n---\nbody"),
        "plain words"
    );
    assert_eq!(d("---\ndescription: \"quoted: yes\"\n---\n"), "quoted: yes");
    assert_eq!(
        d("---\ndescription: >\n  folded\n  lines\nname: a\n---\n"),
        "folded lines"
    );
    assert_eq!(d("---\ndescription: |\n  block\n---\n"), "block");
    assert_eq!(d("no frontmatter"), "");
}

#[test]
fn package_must_be_locked_and_unchanged() {
    let root = ws("lock");
    let dir = install_pkg(&root, "foo");
    let r = Resolver::new(root.clone());
    assert!(err(verdict(&root, "foo", &dir)).contains("not locked"));
    // Approval records the exact content, manifest (and so capabilities) included.
    let msg = r.approve("foo").unwrap();
    assert!(msg.starts_with("approved foo 1.0"), "{msg}");
    let lock = std::fs::read_to_string(root.join("hx.lock")).unwrap();
    assert!(
        lock.contains("name = \"foo\"") && lock.contains("version = \"1.0\""),
        "{lock}"
    );
    assert!(!lock.contains("grants"), "{lock}");
    assert!(verdict(&root, "foo", &dir).is_ok());
    // any content change invalidates the lock
    std::fs::write(dir.join("init.luau"), "return { x = 2 }\n").unwrap();
    assert!(err(verdict(&root, "foo", &dir)).contains("changed since it was approved"));
    // so does a version that differs from the locked one
    std::fs::write(
        dir.join("hx.toml"),
        "[package]\nversion = \"2.0\"\n\n[capabilities]\nfs = [\"read\"]\n",
    )
    .unwrap();
    // lock the new content under the old version: only the version differs
    let digest = snapshot(Location::Package(dir.clone())).unwrap().digest;
    std::fs::write(root.join("hx.lock"), format!("[[package]]\nname = \"foo\"\nversion = \"1.0\"\nkind = \"package\"\nhash = {digest:?}\n")).unwrap();
    assert!(err(verdict(&root, "foo", &dir)).contains("changed since it was approved"));
    // an entry that does not say what it approved (written before entries recorded it) matches nothing
    std::fs::write(
        root.join("hx.lock"),
        format!("[[package]]\nname = \"foo\"\nversion = \"2.0\"\nhash = {digest:?}\n"),
    )
    .unwrap();
    assert!(err(verdict(&root, "foo", &dir)).contains("changed since it was approved"));
    std::fs::write(root.join("hx.lock"), format!("[[package]]\nname = \"foo\"\nversion = \"2.0\"\nkind = \"package\"\nhash = {digest:?}\n")).unwrap();
    assert!(verdict(&root, "foo", &dir).is_ok());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn capabilities_come_from_the_locked_manifest() {
    let root = ws("grants");
    let dir = install_pkg(&root, "foo");
    let r = Resolver::new(root.clone());
    r.approve("foo").unwrap();
    // A lock written before grants were dropped still parses; its extra table is ignored.
    let lock = std::fs::read_to_string(root.join("hx.lock")).unwrap();
    std::fs::write(
        root.join("hx.lock"),
        format!("{lock}\n[package.grants]\nfs = [\"read\"]\n"),
    )
    .unwrap();
    assert!(verdict(&root, "foo", &dir).is_ok());
    // Requesting more is a manifest edit, so it changes the digest and needs a new approval.
    std::fs::write(
        dir.join("hx.toml"),
        "[package]\nversion = \"1.0\"\n\n[capabilities]\nfs = [\"read\", \"write\"]\n",
    )
    .unwrap();
    assert!(err(verdict(&root, "foo", &dir)).contains("changed since it was approved"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_misspelt_capability_or_table_is_a_load_error() {
    let parse = |text: &str| toml::from_str::<Manifest>(text).map_err(|e| e.to_string());
    assert!(
        parse("[package]\nversion = \"1\"\n[capabilities]\nfs = [\"read\"]\nllm = [\"*\"]\n")
            .unwrap()
            .capabilities
            .contains(crate::supervisor::Capability::LlmChat)
    );
    assert!(
        parse("[package]\nversion = \"1\"\n[capabilities]\nfs = [\"raed\"]\n")
            .unwrap_err()
            .contains("unknown capability fs.raed")
    );
    assert!(
        parse("[package]\nversion = \"1\"\n[capabilities]\nprocess = [\"sh\"]\n")
            .unwrap_err()
            .contains("unknown capability process.sh")
    );
    assert!(parse("[package]\nversion = \"1\"\n[capabilites]\nfs = [\"read\"]\n").is_err());
    // Older generated manifests carry [exports] and [tests]; they still load.
    assert!(parse("[package]\nversion = \"1\"\n[exports]\nmodule = \"init.luau\"\n[tests]\nfiles = [\"tests/*.luau\"]\n").is_ok());
}

#[test]
fn approve_keeps_other_entries_and_rejects_unknown_names() {
    let root = ws("approve");
    install_pkg(&root, "foo");
    install_pkg(&root, "bar");
    let r = Resolver::new(root.clone());
    r.approve("foo").unwrap();
    r.approve("bar").unwrap();
    r.approve("foo").unwrap();
    let lock = std::fs::read_to_string(root.join("hx.lock")).unwrap();
    assert_eq!(lock.matches("[[package]]").count(), 2);
    assert!(
        lock.find("\"bar\"").unwrap() < lock.find("\"foo\"").unwrap(),
        "sorted by name: {lock}"
    );
    assert!(err(r.approve("nope")).contains("no package or skill named nope"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn names_are_validated_and_std_cannot_be_shadowed() {
    let root = ws("names");
    for bad in ["std", "project", "my.pkg", "a:b", "Upper", "-x", ""] {
        assert!(!valid_package_name(bad), "{bad}");
    }
    assert!(valid_package_name("video-craft") && valid_package_name("tool_2"));
    let r = Resolver::new(root.clone());
    for bad in ["std", "my.pkg", "a:b"] {
        install_pkg(&root, bad);
        assert!(
            err(r.approve(bad)).contains("not a valid package name"),
            "{bad}"
        );
    }
    std::fs::write(root.join("hx.lock"), "").unwrap();
    // an installed package named std neither replaces nor breaks the built-in package
    let registry = trusted_registry(&root);
    assert_eq!(registry.resolve("std").unwrap().origin, "std/init.luau");
    assert!(registry.resolve("std").unwrap().pkg.is_none());
    for bad in ["std", "my.pkg", "a:b"] {
        assert!(
            registry.rejected[bad].contains("not a valid package name"),
            "{bad}"
        );
        assert!(!registry.capabilities.contains_key(bad), "{bad}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn only_the_built_in_std_package_is_tested_as_the_built_in_suite() {
    let root = ws("teststd");
    let r = Resolver::new(root.clone());
    let is_harness =
        |dir: &Path| matches!(r.test_target(dir), Ok(TestTarget::Harness { note: None }));
    // this repository's stdlib/std, and a copy of the embedded files
    assert!(is_harness(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("stdlib/std")
            .canonicalize()
            .unwrap()
    ));
    let copy = root.join("copy");
    for (_, file, text) in EMBEDDED.iter().filter(|(_, file, _)| *file != "system.md") {
        write(copy.join(file), text);
    }
    assert!(is_harness(&copy));
    // any other directory that calls itself std is refused
    let fake = root.join("fake");
    write(
        fake.join("hx.toml"),
        "[package]\nname = \"std\"\nversion = \"1\"\n",
    );
    write(fake.join("tests/a.luau"), "print(sh('id'))\n");
    assert!(err(r.test_target(&fake)).contains("not the built-in std package"));
    std::fs::write(copy.join("loop.luau"), "return {}\n").unwrap();
    assert!(err(r.test_target(&copy)).contains("not the built-in std package"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn frozen_registry_resolves_overrides_packages_and_builtins() {
    let root = ws("resolve");
    let r = Resolver::new(root.clone());
    let registry = trusted_registry(&root);
    let source = registry.resolve("std/printer").unwrap();
    assert_eq!(
        (
            source.origin.as_str(),
            source.is_override,
            source.pkg.is_none()
        ),
        ("std/printer.luau", false, true)
    );
    assert_eq!(registry.resolve("std").unwrap().origin, "std/init.luau");
    assert!(err(registry.resolve("nope/x")).contains("module not found: nope/x"));
    assert!(err(registry.resolve("project/init")).contains("module not found"));
    write(root.join(".harness/init.luau"), "-- init\n");
    write(
        root.join(".harness/override/std/printer.luau"),
        "return {}\n",
    );
    install_pkg(&root, "foo");
    assert!(err(trusted_registry(&root).resolve("foo")).contains("not locked"));
    r.approve("foo").unwrap();
    let registry = trusted_registry(&root);
    assert_eq!(registry.resolve("project/init").unwrap().text, "-- init\n");
    let source = registry.resolve("std/printer").unwrap();
    assert!(source.is_override && source.text == "return {}\n");
    assert!(!registry.resolve("base:std/printer").unwrap().is_override);
    assert_eq!(
        registry.resolve("foo").unwrap().pkg.unwrap().1,
        crate::supervisor::Capabilities::from([crate::supervisor::Capability::FsRead])
    );
    assert!(err(registry.resolve("foo/missing")).contains("module not found"));
    write(
        root.join(".harness/override/std/printer.luau"),
        "return {changed=true}\n",
    );
    assert_eq!(registry.resolve("std/printer").unwrap().text, "return {}\n");
    // untrusted config: only the built-in package
    let untrusted = Registry::build(&root, None, &crate::dirs::TestDirs::new()).unwrap();
    assert!(
        !untrusted.resolve("std/printer").unwrap().is_override && untrusted.resolve("foo").is_err()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn overrides_replace_built_in_files_only() {
    let root = ws("overrides");
    write(
        root.join(".harness/override/std/system.md"),
        "You are PLANTED.\n",
    );
    write(
        root.join(".harness/override/std/nosuch.luau"),
        "return {}\n",
    );
    write(root.join(".harness/override/foo/init.luau"), "return {}\n");
    install_pkg(&root, "foo");
    Resolver::new(root.clone()).approve("foo").unwrap();
    let registry = trusted_registry(&root);
    assert_eq!(
        registry.asset("std/system.md").unwrap(),
        "You are PLANTED.\n"
    );
    assert!(registry.resolve("std/nosuch").is_err());
    // the package keeps its name: an override cannot replace it
    let foo = registry.resolve("foo").unwrap();
    assert!(!foo.is_override && foo.pkg.is_some(), "{foo:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_registry_is_built_from_the_bytes_trust_read() {
    let root = ws("trustbytes");
    install_pkg(&root, "foo");
    Resolver::new(root.clone()).approve("foo").unwrap();
    let config = crate::trust::Config::read(&root).unwrap();
    // hx.lock changes after trust read it: the registry still follows the bytes that were judged
    std::fs::write(root.join("hx.lock"), "").unwrap();
    let registry = Registry::build(&root, Some(config), &crate::dirs::TestDirs::new()).unwrap();
    assert!(registry.resolve("foo").is_ok());
    assert!(trusted_registry(&root).resolve("foo").is_err());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_digest_frames_paths_and_contents() {
    // Without length framing, `a\0x\0b\0y\0` is both one file a = "x\0b\0y" and two files a = "x", b = "y".
    let one = BTreeMap::from([("a".to_string(), b"x\0b\0y".to_vec())]);
    let two = BTreeMap::from([
        ("a".to_string(), b"x".to_vec()),
        ("b".to_string(), b"y".to_vec()),
    ]);
    assert_ne!(digest(Kind::Package, &one), digest(Kind::Package, &two));
    let moved = BTreeMap::from([
        ("a".to_string(), b"xb".to_vec()),
        ("b".to_string(), b"".to_vec()),
    ]);
    let kept = BTreeMap::from([
        ("a".to_string(), b"x".to_vec()),
        ("b".to_string(), b"b".to_vec()),
    ]);
    assert_ne!(digest(Kind::Package, &moved), digest(Kind::Package, &kept));
    // the same files approved as a skill and as a package are different approvals
    assert_ne!(digest(Kind::Package, &one), digest(Kind::AgentSkill, &one));
}

#[test]
fn an_agent_skill_has_no_luau_and_no_capabilities() {
    let root = ws("agentskill");
    let dir = root.join(".agents/skills/foo");
    write(
        dir.join("SKILL.md"),
        "---\ndescription: helpful\n---\nbody\n",
    );
    write(
        dir.join("hx.toml"),
        "[package]\nversion = \"1\"\nkind = \"tool\"\n[capabilities]\nproc = [\"sh\"]\n",
    );
    write(
        dir.join("init.luau"),
        "return { run = function() return sh('id') end }\n",
    );
    let msg = Resolver::new(root.clone()).approve("foo").unwrap();
    assert!(
        msg.contains("no Luau and no capabilities") && msg.contains("hx.toml is an ordinary file"),
        "{msg}"
    );
    assert!(!msg.contains(".harness/pkgs"), "{msg}");
    let registry = trusted_registry(&root);
    assert!(registry.capabilities["foo"].0.is_empty());
    assert!(registry.resolve("foo").is_err());
    let info = registry.packages.iter().find(|p| p.name == "foo").unwrap();
    assert_eq!(
        (
            info.kind.as_str(),
            info.version.as_str(),
            info.description.as_str()
        ),
        ("skill", "agent-skill", "helpful")
    );
    assert_eq!(info.files, ["SKILL.md", "hx.toml", "init.luau"]);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_skill_approval_does_not_approve_the_same_files_as_a_package() {
    let root = ws("skillaspkg");
    let skill = root.join(".agents/skills/foo");
    write(
        skill.join("SKILL.md"),
        "---\ndescription: inert\n---\nbody\n",
    );
    // the manifest claims the version every agent skill gets, so only the kind tells the two approvals apart
    write(
        skill.join("hx.toml"),
        "[package]\nversion = \"agent-skill\"\nkind = \"tool\"\n[capabilities]\nproc = [\"sh\"]\n",
    );
    write(
        skill.join("init.luau"),
        "return { run = function() return sh('id') end }\n",
    );
    Resolver::new(root.clone()).approve("foo").unwrap();
    // a shell command copies the approved skill into .harness/pkgs (which it can write)
    let pkg = root.join(".harness/pkgs/foo");
    for file in ["SKILL.md", "hx.toml", "init.luau"] {
        write(
            pkg.join(file),
            &std::fs::read_to_string(skill.join(file)).unwrap(),
        );
    }
    assert!(
        err(verdict(&root, "foo", &pkg)).contains("approved as an agent skill, not as a package")
    );
    let registry = trusted_registry(&root);
    assert!(registry.resolve("foo").is_err() && !registry.capabilities.contains_key("foo"));
    assert!(
        registry.rejected["foo"].contains("hx approve foo"),
        "{:?}",
        registry.rejected
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn one_walker_skips_dot_entries_follows_a_linked_root_and_refuses_inner_links() {
    let root = ws("walker");
    let real = root.join("elsewhere/g");
    write(real.join("SKILL.md"), "---\ndescription: g\n---\ng\n");
    write(real.join(".git/HEAD"), "ref: refs/heads/main\n");
    std::fs::create_dir_all(root.join(".agents/skills")).unwrap();
    std::os::unix::fs::symlink(&real, root.join(".agents/skills/g")).unwrap();
    let r = Resolver::new(root.clone());
    assert!(r.approve("g").unwrap().contains("a link to"));
    // what approve locked is what a run loads
    let registry = trusted_registry(&root);
    assert_eq!(
        registry
            .packages
            .iter()
            .find(|p| p.name == "g")
            .unwrap()
            .files,
        ["SKILL.md"]
    );
    assert!(
        r.skills_status(&crate::dirs::TestDirs::new())
            .unwrap()
            .lines()
            .any(|l| l.split_whitespace().take(2).eq(["g", "approved"]))
    );
    // .git churn (git gc) does not un-approve it
    write(real.join(".git/objects/pack/p1"), "pack");
    assert!(
        trusted_registry(&root)
            .packages
            .iter()
            .any(|p| p.name == "g")
    );
    // a link inside is refused by approve and by the run alike
    std::os::unix::fs::symlink("/etc/hostname", real.join("leak.md")).unwrap();
    assert!(err(r.approve("g")).contains("may not contain symlinks"));
    let registry = trusted_registry(&root);
    assert!(
        registry.packages.is_empty() && registry.rejected["g"].contains("may not contain symlinks")
    );
    let _ = std::fs::remove_dir_all(&root);
}

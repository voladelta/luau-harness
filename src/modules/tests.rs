use super::frontmatter_description as d;
use super::*;
use crate::dirs::TestDirs;
use crate::trust::{self, Config};
use anyhow::Result;
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
fn names_are_validated_and_std_cannot_be_shadowed() {
    let root = ws("names");
    for bad in ["std", "project", "my.pkg", "a:b", "Upper", "-x", ""] {
        assert!(!valid_package_name(bad), "{bad}");
    }
    assert!(valid_package_name("video-craft") && valid_package_name("tool_2"));
    for bad in ["std", "my.pkg", "a:b"] {
        install_pkg(&root, bad);
    }
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
fn only_this_binarys_std_runs_the_built_in_suite_and_only_its_embedded_tests() {
    let root = ws("teststd");
    let r = Resolver::new(root.clone());
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("stdlib/std")
        .canonicalize()
        .unwrap();
    assert!(matches!(r.test_target(&source), Ok(TestTarget::Builtin)));
    // the embedded suite is the source tree's, file for file
    let mut on_disk = test_files(&source).unwrap();
    on_disk.sort();
    let embedded: Vec<(String, String)> = EMBEDDED_TESTS
        .iter()
        .map(|(f, t)| (f.to_string(), t.to_string()))
        .collect();
    assert_eq!(
        on_disk, embedded,
        "embed every stdlib/std/tests file in EMBEDDED_TESTS"
    );
    // an exact copy of std with a planted test, or a directory that only claims the name, is refused
    let copy = root.join("std");
    for (_, file, text) in EMBEDDED.iter().filter(|(_, file, _)| *file != "system.md") {
        write(copy.join(file), text);
    }
    write(
        copy.join("tests/zz_test.luau"),
        "sh('echo pwned > PWNED')\n",
    );
    assert!(err(r.test_target(&copy)).contains("calls itself std"));
    let fake = root.join("fake");
    write(
        fake.join("hx.toml"),
        "[package]\nname = \"std\"\nversion = \"1\"\n",
    );
    assert!(err(r.test_target(&fake)).contains("calls itself std"));
    // renamed, the copy is an ordinary package tested inside its boundary
    write(
        copy.join("hx.toml"),
        "[package]\nname = \"mystd\"\nversion = \"1\"\n",
    );
    assert!(matches!(
        r.test_target(&copy),
        Ok(TestTarget::Package { name, .. }) if name == "mystd"
    ));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn frozen_registry_resolves_overrides_packages_and_builtins() {
    let root = ws("resolve");
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
    let dir = install_pkg(&root, "foo");
    let config = Config::read(&root).unwrap();
    // the package changes after trust read it: the registry still loads the bytes that were judged
    std::fs::write(dir.join("init.luau"), "return { x = 2 }\n").unwrap();
    std::fs::write(
        dir.join("hx.toml"),
        "[package]\nversion = \"1.0\"\n[capabilities]\nproc = [\"sh\"]\n",
    )
    .unwrap();
    let registry = Registry::build(&root, Some(config), &TestDirs::new()).unwrap();
    let foo = registry.resolve("foo").unwrap();
    assert_eq!(foo.text, "return { x = 1 }\n");
    assert_eq!(
        foo.pkg.unwrap().1,
        crate::supervisor::Capabilities::from([crate::supervisor::Capability::FsRead])
    );
    let _ = std::fs::remove_dir_all(&root);
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
    let review = Config::read(&root).unwrap().review();
    assert!(
        review.contains("no Luau and no capabilities (its hx.toml is an ordinary file), 3 files"),
        "{review}"
    );
    assert!(!review.contains("proc=sh"), "{review}");
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
fn one_walker_skips_dot_entries_follows_a_linked_root_and_refuses_inner_links() {
    let root = ws("walker");
    let real = root.join("elsewhere/g");
    write(real.join("SKILL.md"), "---\ndescription: g\n---\ng\n");
    write(real.join(".git/HEAD"), "ref: refs/heads/main\n");
    std::fs::create_dir_all(root.join(".agents/skills")).unwrap();
    std::os::unix::fs::symlink(&real, root.join(".agents/skills/g")).unwrap();
    let r = Resolver::new(root.clone());
    let dirs = TestDirs::new();
    trust::record(&root, &dirs).unwrap();
    // what trust hashed is what a run loads
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
        r.skills_status(&dirs)
            .unwrap()
            .lines()
            .any(|l| l.split_whitespace().take(2).eq(["g", "trusted"]))
    );
    // .git churn (git gc) does not untrust it
    write(real.join(".git/objects/pack/p1"), "pack");
    assert!(trust::is_trusted(&root, &dirs));
    // a link inside is never loaded, and trust no longer holds
    std::os::unix::fs::symlink("/etc/hostname", real.join("leak.md")).unwrap();
    assert!(!trust::is_trusted(&root, &dirs));
    assert!(
        Config::read(&root)
            .unwrap()
            .review()
            .contains("may not contain symlinks")
    );
    let registry = trusted_registry(&root);
    assert!(
        !registry.packages.iter().any(|p| p.name == "g")
            && registry.rejected["g"].contains("may not contain symlinks")
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Trust the workspace as it is now, in `dirs`.
fn trust_now(root: &Path, dirs: &TestDirs) {
    trust::record(root, dirs).unwrap();
    assert!(trust::is_trusted(root, dirs));
}

#[test]
fn an_untrusted_or_edited_package_or_skill_is_never_loaded() {
    let root = ws("untrusted");
    let dirs = TestDirs::new();
    let dir = install_pkg(&root, "foo");
    write(
        root.join(".agents/skills/bar/SKILL.md"),
        "---\ndescription: bar\n---\nbar\n",
    );
    // a workspace that only has a package or a skill still needs trust, and an untrusted one loads neither
    assert!(trust::trusted(&root, &dirs).unwrap().is_none());
    let refusal = err(trust::require_trusted(&root, &dirs));
    assert!(
        refusal.contains("not trusted (.agents/skills/bar, .harness/pkgs/foo)"),
        "{refusal}"
    );
    let registry = Registry::capture(&root, &dirs).unwrap();
    assert!(registry.resolve("foo").is_err());
    assert!(!registry.packages.iter().any(|p| p.name == "bar"));
    trust_now(&root, &dirs);
    let registry = Registry::capture(&root, &dirs).unwrap();
    assert!(registry.resolve("foo").is_ok());
    assert!(registry.packages.iter().any(|p| p.name == "bar"));
    // edits by anyone (the agent's shell commands can write both) untrust the workspace, and the refusal names them
    std::fs::write(
        dir.join("hx.toml"),
        "[package]\nversion = \"1.0\"\n[capabilities]\nproc = [\"sh\"]\n",
    )
    .unwrap();
    write(root.join(".agents/skills/bar/notes.md"), "more\n");
    write(
        root.join(".agents/skills/baz/SKILL.md"),
        "---\ndescription: baz\n---\nbaz\n",
    );
    assert!(trust::trusted(&root, &dirs).unwrap().is_none());
    let refusal = err(trust::require_trusted(&root, &dirs));
    assert!(
        refusal.contains(
            "changed since you trusted it: .agents/skills/bar (changed), .agents/skills/baz (new), .harness/pkgs/foo (changed)"
        ),
        "{refusal}"
    );
    let status = Resolver::new(root.clone()).skills_status(&dirs).unwrap();
    assert!(
        status.contains("bar                          changed since trust")
            && status.contains("baz                          not trusted"),
        "{status}"
    );
    std::fs::remove_dir_all(root.join(".agents/skills/baz")).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(err(trust::require_trusted(&root, &dirs)).contains(".harness/pkgs/foo (removed)"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_skill_copied_into_the_packages_is_a_new_package_to_trust() {
    let root = ws("skillaspkg");
    let dirs = TestDirs::new();
    let skill = root.join(".agents/skills/foo");
    write(
        skill.join("SKILL.md"),
        "---\ndescription: inert\n---\nbody\n",
    );
    write(
        skill.join("hx.toml"),
        "[package]\nversion = \"1\"\nkind = \"tool\"\n[capabilities]\nproc = [\"sh\"]\n",
    );
    write(
        skill.join("init.luau"),
        "return { run = function() return sh('id') end }\n",
    );
    trust_now(&root, &dirs);
    // the trusted skill's hx.toml grants nothing and its Luau is not a module
    let registry = Registry::capture(&root, &dirs).unwrap();
    assert!(registry.capabilities["foo"].0.is_empty() && registry.resolve("foo").is_err());
    // a shell command copies it into .harness/pkgs: that is a new package, and nothing loads until the owner trusts it
    for file in ["SKILL.md", "hx.toml", "init.luau"] {
        write(
            root.join(".harness/pkgs/foo").join(file),
            &std::fs::read_to_string(skill.join(file)).unwrap(),
        );
    }
    assert!(err(trust::require_trusted(&root, &dirs)).contains(".harness/pkgs/foo (new)"));
    let review = Config::read(&root).unwrap().review();
    assert!(
        review.contains(
            "package, kind tool, version 1, capabilities proc=sh, 3 files (.harness/pkgs/foo)"
        ),
        "{review}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn the_trust_review_lists_packages_and_skills_escaped() {
    let root = ws("review");
    write(root.join(".harness/policy.toml"), "# policy \x1b[2J\n");
    write(
        root.join(".harness/pkgs/tool-a/hx.toml"),
        "[package]\nversion = \"2.0\\u001b]0;t\\u0007\"\nkind = \"tool\"\n[capabilities]\nfs = [\"read\", \"write\"]\nllm = [\"chat\"]\n",
    );
    write(root.join(".harness/pkgs/tool-a/init.luau"), "return {}\n");
    write(
        root.join(".harness/pkgs/bad.name/hx.toml"),
        "[package]\nversion = \"1\"\n",
    );
    write(
        root.join(".harness/pkgs/nomanifest/init.luau"),
        "return {}\n",
    );
    write(
        root.join(".agents/skills/s\u{202e}kill/SKILL.md"),
        "---\ndescription: x\n---\n",
    );
    write(
        root.join(".agents/skills/plain/SKILL.md"),
        "---\ndescription: x\n---\n",
    );
    let review = Config::read(&root).unwrap().review();
    for want in [
        "===== .harness/policy.toml\n# policy \\u{1b}[2J\n",
        "===== packages and skills\n",
        "tool-a                   package, kind tool, version 2.0\\u{1b}]0;t\\u{7}, capabilities fs=read+write llm=chat, 2 files (.harness/pkgs/tool-a)",
        "bad.name                 not loaded: \"bad.name\" is not a valid package name",
        "nomanifest               not loaded: ",
        "missing hx.toml",
        "plain                    agent skill, no Luau and no capabilities, 1 file (.agents/skills/plain)",
        "s\\u{202e}kill",
    ] {
        assert!(review.contains(want), "{want:?} in\n{review}");
    }
    assert!(!review.chars().any(|c| c.is_control() && c != '\n'));
    let _ = std::fs::remove_dir_all(&root);
}

//! On a machine that can't give Chrome a sandbox, the launch falls back to
//! `--no-sandbox`, and later launches skip the attempt that is bound to fail.
//! Linux and macOS only: the fake browser is a shell script.
#![cfg(unix)]

use std::process::Command;

use serde_json::Value;

fn tool_exe() -> String {
    env!("CARGO_BIN_EXE_navigera").to_string()
}

/// Same resolver as the tool; `NAVIGERA_REQUIRE_BROWSER=1` (CI) forbids skipping.
fn chrome() -> Option<String> {
    let found = navigera::browser::resolve_executable_for(None, true);
    assert!(
        found.is_some() || std::env::var_os("NAVIGERA_REQUIRE_BROWSER").is_none(),
        "NAVIGERA_REQUIRE_BROWSER is set but no Chrome/Chromium was found"
    );
    found
}

fn run(args: &[&str], runtime_dir: &std::path::Path) -> Value {
    let out = Command::new(tool_exe())
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env_remove("NAVIGERA_NO_SANDBOX")
        .output()
        .expect("run navigera");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!("{args:?}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    })
}

#[test]
fn a_machine_without_a_sandbox_pays_the_failed_launch_once() {
    use std::os::unix::fs::PermissionsExt;
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { geteuid() } == 0 {
        // Root never asks for a sandbox, so there is no attempt to skip.
        return;
    }
    let Some(real) = chrome() else { return };
    let dir = std::env::temp_dir().join(format!("nv-sandbox-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let runtime = dir.join("run");
    std::fs::create_dir_all(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    let calls = dir.join("calls");
    // Chrome on such a machine: dies with its own message unless told
    // --no-sandbox.
    let fake = dir.join("chrome");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\necho \"$*\" >> '{calls}'\ncase \" $* \" in *\" --no-sandbox \"*) exec '{real}' \"$@\";; esac\n\
             echo '[0101/000000.000000:FATAL:zygote_host_impl_linux.cc(127)] No usable sandbox! If you are running on Ubuntu 23.10+ or another Linux distro that has disabled unprivileged user namespaces with AppArmor, see https://chromium.googlesource.com/chromium/src/+/main/docs/security/apparmor-userns-restrictions.md.' >&2\n\
             exit 1\n",
            calls = calls.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let fake = fake.to_str().unwrap().to_string();
    let launches =
        || std::fs::read_to_string(&calls).unwrap_or_default().lines().map(str::to_string).collect::<Vec<_>>();

    for round in 0..2 {
        let name = format!("nv-sandbox-{}-{round}", std::process::id());
        let started = run(&["-s", &name, "--chromium", &fake, "start"], &runtime);
        assert_eq!(started["ok"], true, "{started}");
        let two = run(&["-s", &name, "--raw", "eval", "1+1"], &runtime);
        assert_eq!(two, 2, "round {round}");
        run(&["-s", &name, "quit"], &runtime);
    }
    let launches = launches();
    // First session: the sandboxed attempt, then the fallback. Second: only
    // the launch that works.
    assert_eq!(launches.len(), 3, "{launches:#?}");
    assert!(!launches[0].contains("--no-sandbox"), "{launches:#?}");
    assert!(launches[1].contains("--no-sandbox") && launches[2].contains("--no-sandbox"), "{launches:#?}");
    let notes: Vec<_> = std::fs::read_dir(runtime.join("navigera"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("no-sandbox-"))
        .collect();
    assert_eq!(notes.len(), 1);

    // A different binary (a Chrome update) gets the attempt again.
    let mut script = std::fs::read_to_string(&fake).unwrap();
    script.push_str("# updated\n");
    std::fs::write(&fake, script).unwrap();
    std::fs::write(&calls, "").unwrap();
    let name = format!("nv-sandbox-{}-2", std::process::id());
    assert_eq!(run(&["-s", &name, "--chromium", &fake, "start"], &runtime)["ok"], true);
    assert_eq!(run(&["-s", &name, "--raw", "eval", "1+1"], &runtime), 2);
    run(&["-s", &name, "quit"], &runtime);
    let launches = std::fs::read_to_string(&calls).unwrap();
    assert_eq!(launches.lines().count(), 2, "{launches}");
    let _ = std::fs::remove_dir_all(&dir);
}

# 隔离复现说明

基线：be6a0add0652a1e8ea34f93852d74908c9e81497。下面代码仅使用假凭据及临时目录；不要把环境变量改为真实账号目录。

## 锁消融、账号标记与 Windows 命令解析

将以下内容放入基线副本的 examples/audit_probe.rs，先运行 `cargo build --locked` 生成 CLI，再运行 `cargo run --locked --example audit_probe`。程序使用真实文件锁而非模拟锁；整组约 30 秒。运行完删除临时示例文件。

```rust
use codex_switch::{auth, profile};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::time::{Duration, Instant};
fn main() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    unsafe {
        std::env::set_var("CODEX_HOME", root.path().join("codex"));
        std::env::set_var("CODEX_SWITCH_HOME", root.path().join("switch"));
    }
    let value = |name: &str| {
        let payload = serde_json::json!({"email":format!("{name}@example.invalid"), "https://api.openai.com/auth":{"chatgpt_account_id":name,"chatgpt_user_id":name}});
        serde_json::json!({"tokens":{"id_token":format!("e30.{}.sig",URL_SAFE_NO_PAD.encode(payload.to_string())),"access_token":format!("fake-{name}"),"refresh_token":format!("fake-refresh-{name}"),"account_id":name}})
    };
    let a=value("a"); let b=value("b");
    auth::write_auth(&profile::profile_auth_path("a")?, &a)?;
    auth::write_auth(&profile::profile_auth_path("b")?, &b)?;
    for case in ["baseline", "launch_lock_10s", "auth_lock_10s", "baseline_after"] {
        let holder = match case {
            "launch_lock_10s" => Some(profile::lock_launch_session()?),
            "auth_lock_10s" => Some(profile::lock_live_auth()?),
            _ => None,
        };
        let thread=holder.map(|lock| std::thread::spawn(move || {std::thread::sleep(Duration::from_secs(10)); drop(lock);}));
        let start=Instant::now();
        let result=profile::switch_profile("a");
        println!("{case}: elapsed_ms={} success={}", start.elapsed().as_millis(), result.is_ok());
        result?;
        if let Some(thread)=thread {thread.join().unwrap();}
    }
    {
        use fs4::FileExt;
        let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(auth::app_home()?.join("cache.lock"))?;
        FileExt::lock(&lock)?;
        let holder = std::thread::spawn(move || {std::thread::sleep(Duration::from_secs(10)); drop(lock);});
        let binary = std::env::current_exe()?.parent().unwrap().parent().unwrap().join("codex-switch.exe");
        let start=Instant::now();
        let status=std::process::Command::new(binary).args(["--json", "use", "a"]).output()?;
        println!("cli_cache_lock_10s: elapsed_ms={} success={}",start.elapsed().as_millis(),status.status.success());
        holder.join().unwrap();
    }
    {
        let bin=root.path().join("bin"); std::fs::create_dir_all(&bin)?;
        std::fs::write(bin.join("audit-codex-probe.cmd"), "@echo off\r\necho fixture\r\n")?;
        let bare=std::process::Command::new("audit-codex-probe").env("PATH", &bin).output();
        let explicit=std::process::Command::new(bin.join("audit-codex-probe.cmd")).output();
        println!("windows_cmd: bare_resolves={} explicit_resolves={}",bare.is_ok(),explicit.is_ok());
    }
    profile::save_auth_value(b.clone(), None)?;
    let live=auth::read_auth(&auth::codex_auth_path()?)?;
    println!("existing_login: marker={} live_is_a={} live_is_b={}",profile::read_current(),live==a,live==b);
    Ok(())
}


```

## 同账号启动恢复红灯

在同一基线副本 src/launch.rs 的既有 tests 模块内加入以下测试，复用 TestAppHome/auth_value/staged_launch/read_json fixture。运行 `cargo test --locked --lib audit_same_account_restore_keeps_rotated_live_credentials`。预期当前基线失败在第二条断言；诊断后撤销新增用例，正式修复时再以回归测试纳入。

```rust
#[test]
fn audit_same_account_restore_keeps_rotated_live_credentials() {
    let home = TestAppHome::new();
    let old = auth_value("a", "refresh-old", "2026-07-01T00:00:00Z");
    let (profile_path, live, backup) = staged_launch(&home, &old);
    crate::auth::write_auth(&backup, &old).unwrap();
    let new = auth_value("a", "refresh-new", "2026-07-20T10:00:00Z");
    crate::auth::write_auth(&live, &new).unwrap();
    restore_launch_auth(&live, &backup, true, "work").unwrap();
    assert_eq!(read_json(&profile_path), new);
    assert_eq!(read_json(&live), new,
        "same-account restore must keep rotated credentials live");
}
```

实测：profile 的新值保留，live 被回写为 refresh-old。全程无服务端请求，证明的是文件状态错误，不是在线认证失效测试。

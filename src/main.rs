#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod app;
mod context;
mod overlay;
mod payload;
mod settings;
mod style;
mod visual;
mod wechat;
mod weflow;

fn main() -> std::process::ExitCode {
    let argument = std::env::args().nth(1);
    if argument.is_some() {
        use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
        unsafe {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
    let outcome = match argument.as_deref() {
        Some("--diagnose") => wechat::diagnose(),
        Some("--diagnose-screen") => visual::diagnose(),
        Some("--set-key") => settings::Settings::load().and_then(|settings| {
            let key = rpassword::prompt_password("API Key: ")?;
            settings.save_key(&key)?;
            println!("Windows Credential Manager: saved");
            Ok(())
        }),
        Some("--test-api") => settings::Settings::load().and_then(|settings| {
            let start = std::time::Instant::now();
            let output = app::test_model(&settings)?;
            println!(
                "Stream completed in {} ms: {output}",
                start.elapsed().as_millis()
            );
            Ok(())
        }),
        None => app::run(),
        Some(_) => {
            eprintln!("Supported options: --diagnose, --diagnose-screen, --set-key, --test-api");
            std::process::exit(2);
        }
    };
    if let Err(error) = outcome {
        if argument.is_none() {
            use windows::{
                Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MessageBoxW},
                core::{PCWSTR, w},
            };
            let text = settings::wide(&error.to_string());
            unsafe {
                MessageBoxW(
                    None,
                    PCWSTR(text.as_ptr()),
                    w!("微信消息补全"),
                    MB_ICONERROR,
                );
            }
        } else {
            eprintln!("{error:#}");
        }
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

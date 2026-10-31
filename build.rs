use std::path::PathBuf;

// O mesmo desenho do ícone que o app usa em tempo de execução.
#[path = "src/logo.rs"]
mod logo;

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("falha ao compilar ui/app.slint");
    embed_icon();
}

/// Embute o ícone no .exe, que é o que o Explorer, o menu Iniciar e a barra de tarefas
/// usam antes de o app abrir qualquer janela. Tamanhos até 96 px: o de 256 sozinho
/// somaria 256 KB ao executável.
fn embed_icon() {
    println!("cargo:rerun-if-changed=src/logo.rs");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = out.join("landpad.ico");
    std::fs::write(&ico, logo::ico(&[16, 20, 24, 32, 40, 48, 64, 96])).expect("falha ao gravar o ícone");
    let rc = out.join("landpad.rc");
    let ico_path = ico.display().to_string().replace('\\', "\\\\");
    std::fs::write(&rc, format!("1 ICON \"{ico_path}\"\n")).expect("falha ao gravar o .rc");
    embed_resource::compile(&rc, embed_resource::NONE).manifest_optional().expect("falha ao embutir o ícone");
}

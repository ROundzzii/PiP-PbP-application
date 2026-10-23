// OpenH264 se compile sans ses optimisations SIMD si NASM est absent, et devient alors
// 5 à 30 fois plus lent. On prévient au lieu d'échouer silencieusement.
fn main() {
    let found = std::process::Command::new("nasm").arg("-v").output().is_ok_and(|o| o.status.success());
    if !found {
        println!(
            "cargo:warning=NASM introuvable dans le PATH : l'encodeur H.264 sera TRÈS lent. \
             Installez-le (winget install NASM.NASM) puis `cargo clean -p openh264-sys2`."
        );
    }
    println!("cargo:rerun-if-env-changed=PATH");
}

use std::process::Command;

fn main() {
    //println!("cargo::rerun-if-changed=static/main.css");
    Command::new("bunx").args(["@tailwindcss/cli -i ./static/mainp.css -o ./static/main.css"]).status().unwrap();
}

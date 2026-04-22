mod cli;
mod fitness;
mod halt;
mod hook;
mod r#loop;
mod protocol;
mod session;

fn main() {
    std::process::exit(cli::run());
}

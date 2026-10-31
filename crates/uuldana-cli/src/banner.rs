//! The words of uuldana's header. Drawing is in `frame`.

pub(crate) const TITLE: [&str; 6] = [
    "██╗   ██╗██╗   ██╗██╗     ██████╗  █████╗ ███╗   ██╗ █████╗ ",
    "██║   ██║██║   ██║██║     ██╔══██╗██╔══██╗████╗  ██║██╔══██╗",
    "██║   ██║██║   ██║██║     ██║  ██║███████║██╔██╗ ██║███████║",
    "██║   ██║██║   ██║██║     ██║  ██║██╔══██║██║╚██╗██║██╔══██║",
    "╚██████╔╝╚██████╔╝███████╗██████╔╝██║  ██║██║ ╚████║██║  ██║",
    " ╚═════╝  ╚═════╝ ╚══════╝╚═════╝ ╚═╝  ╚═╝╚═╝  ╚═══╝╚═╝  ╚═╝",
];
pub(crate) const SUBTITLE: &str = "ONE TERMINAL. MULTIPLE AGENTS.";
pub(crate) const HINT: &str = "Type /agent and press Enter to choose which agent works first.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hint_tells_where_to_choose_the_agent() {
        assert!(HINT.contains("/agent"));
    }
}

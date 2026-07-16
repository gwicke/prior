#[test]
fn debug_wrap_help() {
    for s in ["Commands:", "  /help      — Show available commands and key bindings",
              "/thinking   —  Set thinking level : off", "a: alpha model ctx 128000 $0.10/0.20 per M"] {
        let w = crate::output::wrap(s, 100);
        eprintln\!("IN : {:?}\nOUT: {:?}\n", s, w);
    }
}

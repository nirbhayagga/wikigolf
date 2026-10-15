//! Embed readable source files into one self-contained page in both editions.
pub fn render(static_mode: bool) -> String {
    let mut page = include_str!("../static/index.html").to_string();
    for (name, source) in [
        ("game.css", include_str!("../static/game.css")),
        ("race-link.js", include_str!("../static/race-link.js")),
        (
            "puzzle-choice.js",
            include_str!("../static/puzzle-choice.js"),
        ),
        ("boot.js", include_str!("../static/boot.js")),
        ("data.js", include_str!("../static/data.js")),
        ("map.js", include_str!("../static/map.js")),
        ("game.js", include_str!("../static/game.js")),
        ("reader-duels.js", include_str!("../static/reader-duels.js")),
        ("experience.js", include_str!("../static/experience.js")),
        ("init.js", include_str!("../static/init.js")),
    ] {
        let tag = if name.ends_with(".css") {
            "style"
        } else {
            "script"
        };
        page = page.replace(
            &format!("<!-- inline:{name} -->"),
            &format!("<{tag}>\n{source}\n</{tag}>"),
        );
    }
    if static_mode {
        page = page.replacen("<html lang=\"en\">", "<html lang=\"en\" data-static>", 1);
    }
    page
}

fn kotost_roulette_scroll(
    game: &crate::game::Game<'_>,
    screen: &NativeScreenView<'_>,
    scroll: [f64; 2],
    modifiers: u32,
    canceled: &mut bool,
) {
    crate::game::kotost_roulette::scroll(game, screen, scroll, modifiers, canceled);
}

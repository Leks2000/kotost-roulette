fn kotost_roulette_press(
    game: &crate::game::Game<'_>,
    screen: &NativeScreenView<'_>,
    button: i32,
    canceled: &mut bool,
) {
    crate::game::kotost_roulette::press(game, screen, button, canceled);
}

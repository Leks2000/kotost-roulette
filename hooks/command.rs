fn kotost_roulette_command(
    game: &mut crate::game::Game<'_>,
    player: &NativeServerPlayer,
    command: &str,
    ran: &mut bool,
) {
    crate::game::kotost_roulette::perform_command(game, player, command, ran);
}

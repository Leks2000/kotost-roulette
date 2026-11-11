fn kotost_roulette_net_from_server(game: &mut crate::game::Game<'_>, channel: &str, data: &[u8]) {
    crate::game::kotost_roulette::net_from_server(game, channel, data);
}

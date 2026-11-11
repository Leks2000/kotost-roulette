// Kotost Roulette («Рулетка»): stake a stack of game items, spin a wheel, win a dearer item or lose the stake.
// - Every item of the table has a price in points (`config/kotost_roulette.txt`, the server's; ~400 items).
// - The screen (a mod screen over the world): the wheel, the stake slot, the reward slot (a click opens a search
//   of the table), x2 / x4 / x5 / x8 / x10 (the table's item nearest the stake's points times N), the chance
//   (0.996 / multiplier) before the spin, the player's inventory below (a click stakes that stack).
// - The roll is the server's: it takes the stake when it rolls and sends the client only the outcome, which the
//   wheel then shows over 3 s. The reward reaches the inventory when the wheel stops (on the ground when it is
//   full; into the player's file when they left). Leaving mid-spin changes nothing: the stake is already gone.
// Lives in crate::game::kotost_roulette (the game's `game` module), so it reaches the game's helpers.

use super::Game;
use super::native_inventories::NativeInventoryKind;
use horizon_client::hud::HudBatch;
use horizon_client::items::ItemStack;
use horizon_client::mod_api::{Compound, NativeCommand, NativeScreenView, NativeServerPlayer, Tag};
use horizon_client::text::Text;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

pub const MOD: &str = "kotost_roulette";
const CH_OPEN: &str = "kotost_roulette:open";
/// Parts of the table, sent before `open` (a message holds at most 30 000 bytes).
const CH_TABLE: &str = "kotost_roulette:table";
const CH_SPIN: &str = "kotost_roulette:spin";
const CH_RESULT: &str = "kotost_roulette:result";
/// How long the wheel turns; the reward waits as long on the server.
const SPIN_SECS: f32 = 3.0;
const MULTS: [u32; 5] = [2, 4, 5, 8, 10];
/// The item «Рулетка» is: a clock named so.
const WHEEL_ITEM: &str = "minecraft:clock";
const WHEEL_NAMES: [&str; 2] = ["Рулетка", "Roulette"];

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ===================================================================================================
// The table
// ===================================================================================================

#[derive(Clone, Debug)]
struct Price {
    id: String,
    pts: u64,
    name: String,
}

#[derive(Clone, Debug)]
struct Table {
    /// The chance of x1: 0.996 (the house keeps 0.4 %).
    edge: f64,
    min_mult: f64,
    max_mult: f64,
    prices: Vec<Price>,
}

impl Default for Table {
    fn default() -> Self {
        Self { edge: 0.996, min_mult: 1.05, max_mult: 1000.0, prices: Vec::new() }
    }
}

/// An item's name in English, from its id: `minecraft:diamond_sword` → `Diamond Sword`.
fn english(id: &str) -> String {
    id.rsplit(':')
        .next()
        .unwrap_or(id)
        .split('_')
        .map(|w| {
            let mut c = w.chars();
            c.next().map_or(String::new(), |f| f.to_uppercase().collect::<String>() + c.as_str())
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Price {
    /// The table's (Russian) name in Russian, else the English one.
    fn shown(&self, ru: bool) -> String {
        if ru { self.name.clone() } else { english(&self.id) }
    }
}

/// The window's game is in Russian (the client's texts); set each tick from its language.
static CLIENT_RU: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn ru() -> bool {
    CLIENT_RU.load(std::sync::atomic::Ordering::Relaxed)
}

/// The client's text in its language.
fn tr(r: &'static str, e: &'static str) -> &'static str {
    if ru() { r } else { e }
}

impl Table {
    fn price(&self, id: &str) -> Option<&Price> {
        self.prices.iter().find(|p| p.id == id)
    }
}

/// `id = points ; Имя` lines, `#` comments, and `edge`, `min_mult`, `max_mult`. A later line of an item wins.
fn parse(text: &str) -> Table {
    let mut t = Table::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (body, name) = line.split_once(';').map_or((line, ""), |(b, n)| (b.trim(), n.trim()));
        let Some((k, v)) = body.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        let num = v.replace(',', ".").parse::<f64>().ok().filter(|x| x.is_finite());
        match k {
            "edge" => t.edge = num.map_or(t.edge, |x| x.clamp(0.5, 1.0)),
            "min_mult" => t.min_mult = num.map_or(t.min_mult, |x| x.clamp(1.01, 100.0)),
            "max_mult" => t.max_mult = num.map_or(t.max_mult, |x| x.clamp(2.0, 1_000_000.0)),
            _ => {
                let Ok(pts) = v.parse::<u64>() else { continue };
                if pts == 0 || pts > 1_000_000_000 {
                    continue;
                }
                let id = if k.contains(':') { k.to_owned() } else { format!("minecraft:{k}") };
                let name = if name.is_empty() {
                    id.rsplit(':').next().unwrap_or(&id).replace('_', " ")
                } else {
                    name.chars().take(40).collect()
                };
                t.prices.retain(|p| p.id != id);
                t.prices.push(Price { id, pts, name });
            }
        }
    }
    t.prices.sort_by(|a, b| a.pts.cmp(&b.pts).then_with(|| a.name.cmp(&b.name)));
    t
}

/// The table as the server sends it: parts of `id\tpoints\tname` lines (`minecraft:` left out), then the
/// head, `edge\tmin\tmax`.
fn encode_table(t: &Table) -> (Vec<Vec<u8>>, Vec<u8>) {
    let mut parts = Vec::new();
    let mut s = String::new();
    for p in &t.prices {
        let id = p.id.strip_prefix("minecraft:").unwrap_or(&p.id);
        let line = format!("{id}\t{}\t{}\n", p.pts, p.name);
        if s.len() + line.len() > 24_000 {
            parts.push(std::mem::take(&mut s).into_bytes());
        }
        s.push_str(&line);
    }
    if !s.is_empty() {
        parts.push(s.into_bytes());
    }
    (parts, format!("{}\t{}\t{}", t.edge, t.min_mult, t.max_mult).into_bytes())
}

fn decode_prices(data: &[u8], into: &mut Vec<Price>) {
    let Ok(text) = std::str::from_utf8(data) else { return };
    for line in text.lines() {
        let mut f = line.splitn(3, '\t');
        let (Some(id), Some(pts), Some(name)) = (f.next(), f.next(), f.next()) else { continue };
        let Ok(pts) = pts.parse::<u64>() else { continue };
        let id = if id.contains(':') { id.to_owned() } else { format!("minecraft:{id}") };
        if into.len() < 5000 {
            into.push(Price { id, pts, name: name.to_owned() });
        }
    }
}

fn decode_head(data: &[u8], prices: Vec<Price>) -> Option<Table> {
    let text = std::str::from_utf8(data).ok()?;
    let mut head = text.split('\t').map(|v| v.parse::<f64>().ok());
    Some(Table { edge: head.next()??, min_mult: head.next()??, max_mult: head.next()??, prices })
}

fn config_path() -> std::path::PathBuf {
    horizon_client::logging::game_home().join("config").join("kotost_roulette.txt")
}

// ===================================================================================================
// Server
// ===================================================================================================

struct Pending {
    uuid: u128,
    item: String,
    count: u32,
    due: Instant,
}

#[derive(Default)]
struct Server {
    /// The table, the file's time when read, when it was last looked at.
    table: Option<(Table, Option<SystemTime>, Instant)>,
    rng: u64,
    /// When each player's last spin began.
    busy: HashMap<u128, Instant>,
    /// Rewards waiting for the wheel to stop (or for their player to come back).
    pending: Vec<Pending>,
}

static SERVER: Mutex<Option<Server>> = Mutex::new(None);
/// Spins asked for by clients, waiting for the next server tick: (player, message, when).
static QUEUE: Mutex<Vec<(u128, Vec<u8>, Instant)>> = Mutex::new(Vec::new());

/// The server's table: read again when the file changed (looked at every 5 s at most); written with the
/// defaults when there is none. Items this game does not have are left out.
fn table(game: &Game<'_>, s: &mut Server) -> Table {
    let path = config_path();
    if let Some((t, _, at)) = &s.table {
        if at.elapsed() < Duration::from_secs(5) {
            return t.clone();
        }
    }
    let time = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    if let Some((t, was, at)) = &mut s.table {
        if *was == time && time.is_some() {
            *at = Instant::now();
            return t.clone();
        }
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) if path.exists() => DEFAULT_TABLE.to_owned(),
        Err(_) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, DEFAULT_TABLE);
            DEFAULT_TABLE.to_owned()
        }
    };
    let mut t = parse(&text);
    t.prices.retain(|p| game.native_max_stack_size(&p.id).is_some());
    let time = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    s.table = Some((t.clone(), time, Instant::now()));
    t
}

fn rand01(rng: &mut u64, salt: u128) -> f64 {
    if *rng == 0 {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(salt);
        h.write_u128(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(1, |d| d.as_nanos()));
        *rng = h.finish() | 1;
    }
    // xorshift64*, mixed with the time of each spin.
    *rng ^= SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos() as u64) << 20;
    *rng ^= *rng >> 12;
    *rng ^= *rng << 25;
    *rng ^= *rng >> 27;
    let v = rng.wrapping_mul(0x2545_F491_4F6C_DD1D);
    (v >> 11) as f64 / (1u64 << 53) as f64
}

fn tell(game: &mut Game<'_>, uuid: u128, line: &str, color: u32) {
    let _ = game.native_tell(uuid, Text::literal(line).with_color(color));
}

/// Whether that player's game is in Russian (else English).
fn is_ru(game: &Game<'_>, uuid: u128) -> bool {
    game.native_player_client(uuid).is_some_and(|i| i.language.to_lowercase().starts_with("ru"))
}

fn refuse(game: &mut Game<'_>, uuid: u128, ru: &str, en: &str) {
    let why = if is_ru(game, uuid) { ru } else { en };
    let mut data = vec![0u8];
    data.extend_from_slice(why.as_bytes());
    let _ = game.native_net_send_to_player(uuid, CH_RESULT, &data);
}

/// Opens the screen on that player's client: sends the table.
fn open_for(game: &mut Game<'_>, uuid: u128) {
    let t = {
        let mut guard = lock(&SERVER);
        let s = guard.get_or_insert_with(Server::default);
        table(game, s)
    };
    let (parts, head) = encode_table(&t);
    let sent = parts.iter().all(|p| game.native_net_send_to_player(uuid, CH_TABLE, p).is_ok())
        && game.native_net_send_to_player(uuid, CH_OPEN, &head).is_ok();
    if !sent {
        let line = if is_ru(game, uuid) {
            "Рулетке нужен клиент Horizon с модом kotost_roulette"
        } else {
            "The roulette needs a Horizon client with the kotost_roulette mod"
        };
        tell(game, uuid, line, 0xFF5555);
    }
}

/// `native_register_commands`.
pub fn register_commands(commands: &mut Vec<NativeCommand>) {
    commands.push(NativeCommand::new("roulette"));
    commands.push(NativeCommand::new("roulette give"));
}

/// `native_perform_command`: /roulette opens the screen, /roulette give gives the item «Рулетка».
pub fn perform_command(game: &mut Game<'_>, player: &NativeServerPlayer, command: &str, ran: &mut bool) {
    let mut words = command.split_whitespace();
    if words.next() != Some("roulette") {
        return;
    }
    *ran = true;
    let ru = is_ru(game, player.uuid);
    let tr = |r: &'static str, e: &'static str| if ru { r } else { e };
    match words.next() {
        None => open_for(game, player.uuid),
        Some("give") => {
            let stack = wheel_stack(game, ru);
            if game.native_give_stack(player.uuid, stack).is_some() {
                game.native_feedback(Text::literal(tr("Нет места в инвентаре", "No room in the inventory")).with_color(0xFF5555));
            } else {
                game.native_feedback(
                    Text::literal(tr("Рулетка в инвентаре: ПКМ с ней — открыть колесо", "Roulette is in your inventory: right-click with it to open the wheel")).with_color(0xFFAA00),
                );
            }
        }
        _ => game.native_feedback(
            Text::literal(tr("/roulette — открыть Рулетку, /roulette give — получить предмет «Рулетка»", "/roulette opens the Roulette, /roulette give gives the Roulette item"))
                .with_color(0xFFFF55),
        ),
    }
}

fn wheel_stack(game: &Game<'_>, ru: bool) -> ItemStack {
    let (name, lore) = if ru { ("Рулетка", "ПКМ — открыть колесо") } else { ("Roulette", "Right-click to open the wheel") };
    let fancy = format!(
        r#"{{id:"minecraft:clock",count:1,components:{{"minecraft:custom_name":{{text:"{name}",color:"gold",italic:false}},"minecraft:lore":[{{text:"{lore}",color:"gray",italic:false}}],"minecraft:enchantment_glint_override":true}}}}"#
    );
    let plain = format!(r#"{{id:"minecraft:clock",count:1,components:{{"minecraft:custom_name":"{name}"}}}}"#);
    [fancy, plain]
        .iter()
        .find_map(|snbt| game.native_item_from_snbt(snbt))
        .unwrap_or_else(|| ItemStack::new(WHEEL_ITEM, 1))
}

fn is_wheel(game: &Game<'_>, stack: &ItemStack) -> bool {
    if &*stack.item != WHEEL_ITEM || stack.extra.is_none() {
        return false;
    }
    let name = game.stack_display_name(stack).plain();
    WHEEL_NAMES.iter().any(|n| name.contains(n))
}

/// `native_use_item` (server): a right click with «Рулетка» opens the screen.
pub fn use_item(game: &mut Game<'_>, stack: &ItemStack, claimed: &mut bool) {
    if &*stack.item != WHEEL_ITEM || stack.extra.is_none() || game.native_window() {
        return;
    }
    if !is_wheel(game, stack) {
        return;
    }
    let Some(p) = game.native_acting_player() else { return };
    *claimed = true;
    open_for(game, p.uuid);
}

/// `native_net_from_client`: a client asks for a spin; it waits for the next tick.
pub fn net_from_client(player: u128, channel: &str, data: &[u8]) {
    if channel != CH_SPIN || data.len() > 512 {
        return;
    }
    let mut q = lock(&QUEUE);
    if q.len() < 256 {
        q.push((player, data.to_vec(), Instant::now()));
    }
}

/// A spin as the client asks for it.
struct Ask {
    slot: usize,
    bet_count: u32,
    target_count: u32,
    bet: String,
    target: String,
}

fn encode_ask(a: &Ask) -> Vec<u8> {
    let mut d = vec![a.slot as u8];
    d.extend_from_slice(&(a.bet_count as u16).to_le_bytes());
    d.extend_from_slice(&(a.target_count as u16).to_le_bytes());
    for s in [&a.bet, &a.target] {
        d.push(s.len().min(255) as u8);
        d.extend_from_slice(&s.as_bytes()[..s.len().min(255)]);
    }
    d
}

fn decode_ask(d: &[u8]) -> Option<Ask> {
    let slot = usize::from(*d.first()?);
    let bet_count = u32::from(u16::from_le_bytes([*d.get(1)?, *d.get(2)?]));
    let target_count = u32::from(u16::from_le_bytes([*d.get(3)?, *d.get(4)?]));
    let mut at = 5;
    let mut read = || {
        let n = usize::from(*d.get(at)?);
        let s = std::str::from_utf8(d.get(at + 1..at + 1 + n)?).ok()?.to_owned();
        at += 1 + n;
        Some(s)
    };
    let bet = read()?;
    let target = read()?;
    Some(Ask { slot, bet_count, target_count, bet, target })
}

/// `native_tick` on the server: the spins asked for, then the rewards whose wheel has stopped.
/// Players who joined or respawned: each gets one «Рулетка» when they have none (tried for 10 s).
static ARRIVED: Mutex<Vec<(u128, Instant)>> = Mutex::new(Vec::new());

/// `native_player_logged_in` / `native_player_respawned`.
pub fn arrived(player: u128) {
    let mut a = lock(&ARRIVED);
    if a.len() < 256 && !a.iter().any(|(u, _)| *u == player) {
        a.push((player, Instant::now()));
    }
}

/// One «Рулетка» for each player who arrived and carries none (nothing while they are dead or logging in).
fn hand_out(game: &mut Game<'_>) {
    let list: Vec<(u128, Instant)> = std::mem::take(&mut *lock(&ARRIVED));
    let mut keep = Vec::new();
    for (uuid, at) in list {
        let Ok(inv) = game.native_inventory(uuid, NativeInventoryKind::Player) else {
            if at.elapsed() < Duration::from_secs(10) {
                keep.push((uuid, at));
            }
            continue;
        };
        if !inv.online {
            continue;
        }
        if inv.slots.iter().flatten().any(|st| is_wheel(game, st)) {
            continue;
        }
        let ru = is_ru(game, uuid);
        let _ = game.native_give_stack(uuid, wheel_stack(game, ru));
    }
    lock(&ARRIVED).extend(keep);
}

fn server_tick(game: &mut Game<'_>) {
    if !lock(&ARRIVED).is_empty() {
        hand_out(game);
    }
    let asks: Vec<(u128, Vec<u8>)> = {
        let mut q = lock(&QUEUE);
        if q.is_empty() && lock(&SERVER).as_ref().is_none_or(|s| s.pending.is_empty()) {
            return;
        }
        q.drain(..).filter(|(_, _, at)| at.elapsed() < Duration::from_secs(5)).map(|(u, d, _)| (u, d)).collect()
    };
    let mut guard = lock(&SERVER);
    let s = guard.get_or_insert_with(Server::default);
    let players = game.native_server_players();
    for (uuid, data) in asks {
        if let Some(ask) = decode_ask(&data) {
            if players.iter().any(|p| p.uuid == uuid) {
                spin(game, s, uuid, &ask);
            }
        }
    }
    deliver(game, s, &players);
}

fn spin(game: &mut Game<'_>, s: &mut Server, uuid: u128, a: &Ask) {
    if s.busy.get(&uuid).is_some_and(|t| t.elapsed().as_secs_f32() < SPIN_SECS + 0.2) {
        return refuse(game, uuid, "Колесо ещё крутится", "The wheel is still turning");
    }
    let t = table(game, s);
    let Some(bp) = t.price(&a.bet).map(|p| p.pts) else {
        return refuse(game, uuid, "У этого предмета нет цены — его нельзя поставить", "This item has no price: it can't be staked");
    };
    let Some(tp) = t.price(&a.target).map(|p| p.pts) else {
        return refuse(game, uuid, "У награды нет цены", "The reward has no price");
    };
    let max = game.native_max_stack_size(&a.target).unwrap_or(64).max(1);
    if a.bet_count == 0 || a.target_count == 0 || a.target_count > max * 36 || a.slot >= 36 {
        return refuse(game, uuid, "Неверная ставка", "Wrong stake");
    }
    let Ok(inv) = game.native_inventory(uuid, NativeInventoryKind::Player) else {
        return refuse(game, uuid, "Сейчас нельзя", "Not now");
    };
    let Some(stack) = inv.slots.get(a.slot).cloned().flatten() else {
        return refuse(game, uuid, "Слот ставки пуст", "The stake slot is empty");
    };
    if *stack.item != *a.bet || stack.count < a.bet_count {
        return refuse(game, uuid, "Ставка изменилась", "The stake changed");
    }
    if stack.damage != 0 || stack.extra.is_some() {
        return refuse(game, uuid, "Только обычные предметы: без чар, имён и износа", "Plain items only: no enchantments, names or wear");
    }
    let mult = (tp * u64::from(a.target_count)) as f64 / (bp * u64::from(a.bet_count)) as f64;
    if mult < t.min_mult {
        return refuse(game, uuid, "Награда должна быть дороже ставки", "The reward must be worth more than the stake");
    }
    if mult > t.max_mult {
        return refuse(game, uuid, &format!("Слишком большой множитель: не больше x{}", t.max_mult), &format!("Multiplier too high: x{} at most", t.max_mult));
    }
    let chance = (t.edge / mult).min(1.0);
    let rest = (stack.count > a.bet_count).then(|| ItemStack { count: stack.count - a.bet_count, ..stack.clone() });
    if game.native_set_inventory_slot(uuid, NativeInventoryKind::Player, a.slot, Some(&stack), rest).is_err() {
        return refuse(game, uuid, "Ставка изменилась", "The stake changed");
    }
    let roll = rand01(&mut s.rng, uuid);
    let win = roll < chance;
    s.busy.insert(uuid, Instant::now());
    if win {
        s.pending.push(Pending {
            uuid,
            item: a.target.clone(),
            count: a.target_count,
            due: Instant::now() + Duration::from_secs_f32(SPIN_SECS + 0.1),
        });
    }
    let mut data = vec![1u8, u8::from(win)];
    data.extend_from_slice(&(roll as f32).to_le_bytes());
    data.extend_from_slice(&(chance as f32).to_le_bytes());
    let _ = game.native_net_send_to_player(uuid, CH_RESULT, &data);
}

fn deliver(game: &mut Game<'_>, s: &mut Server, players: &[NativeServerPlayer]) {
    if s.pending.is_empty() {
        return;
    }
    let now = Instant::now();
    s.busy.retain(|_, t| t.elapsed() < Duration::from_secs(10));
    let mut keep = Vec::new();
    for pd in std::mem::take(&mut s.pending) {
        if pd.due > now {
            keep.push(pd);
            continue;
        }
        let max = game.native_max_stack_size(&pd.item).unwrap_or(64).max(1);
        let mut left = pd.count;
        match players.iter().find(|p| p.uuid == pd.uuid) {
            Some(p) if p.health > 0.0 => {
                while left > 0 {
                    let n = left.min(max);
                    left -= n;
                    if let Some(back) = game.native_give_stack(p.uuid, ItemStack::new(&pd.item, n)) {
                        if !drop_at(game, p, &pd.item, back.count) {
                            left += back.count;
                            break;
                        }
                    }
                }
                if left < pd.count {
                    let ru = is_ru(game, p.uuid);
                    let name = table(game, s).price(&pd.item).map_or_else(|| english(&pd.item), |p| p.shown(ru));
                    let line = if ru {
                        format!("Рулетка: выигрыш — {} × {name}", pd.count - left)
                    } else {
                        format!("Roulette: you won {} × {name}", pd.count - left)
                    };
                    tell(game, p.uuid, &line, 0x55FF55);
                }
            }
            Some(_) => {}
            None => left = into_file(game, pd.uuid, &pd.item, left, max),
        }
        if left > 0 {
            keep.push(Pending { count: left, due: now + Duration::from_secs(2), ..pd });
        }
    }
    s.pending = keep;
}

/// What did not fit, on the ground at the player's feet. False when it could not be dropped.
fn drop_at(game: &mut Game<'_>, p: &NativeServerPlayer, item: &str, count: u32) -> bool {
    let mut it = Compound::new();
    it.insert("id".into(), Tag::String(item.into()));
    it.insert("count".into(), Tag::Int(count as i32));
    let mut c = Compound::new();
    c.insert("Item".into(), Tag::Compound(it));
    c.insert("PickupDelay".into(), Tag::Short(10));
    game.native_summon(&p.dimension, "minecraft:item", p.position, c).is_some()
}

/// A player who left: the reward into the empty main slots of their file; what is left.
fn into_file(game: &mut Game<'_>, uuid: u128, item: &str, mut left: u32, max: u32) -> u32 {
    let Ok(inv) = game.native_inventory(uuid, NativeInventoryKind::Player) else { return left };
    for slot in 0..36 {
        if left == 0 {
            break;
        }
        if inv.slots.get(slot).is_some_and(Option::is_none) {
            let n = left.min(max);
            let put = game.native_set_inventory_slot(
                uuid,
                NativeInventoryKind::Player,
                slot,
                None,
                Some(ItemStack::new(item, n)),
            );
            if put.is_ok() {
                left -= n;
            }
        }
    }
    left
}

/// `native_tick`: the server's part on a server, the client's in the window.
pub fn tick(game: &mut Game<'_>) {
    if game.native_window() {
        client_tick(game);
    } else {
        server_tick(game);
    }
}

// ===================================================================================================
// Client: the screen
// ===================================================================================================

const W: f32 = 302.0;
const H: f32 = 250.0;
const SLOT: f32 = 18.0;
/// The top block: two item panels and the ring between them.
const TOP_Y: f32 = 20.0;
const TOP_H: f32 = 96.0;
const PANEL_W: f32 = 90.0;
const LEFT: [f32; 4] = [6.0, TOP_Y, PANEL_W, TOP_H];
const RIGHT: [f32; 4] = [W - 6.0 - PANEL_W, TOP_Y, PANEL_W, TOP_H];
/// The ring: its centre and radii.
const WX: f32 = W / 2.0;
const WY: f32 = TOP_Y + TOP_H / 2.0;
const WR: f32 = 40.0;
const WI: f32 = 30.0;
/// The button row: the spin button, then x2 x4 x5 x8 x10 and 30 % 50 % 70 %.
const SPIN_BTN: [f32; 4] = [6.0, 121.0, 84.0, 18.0];
const CHANCES: [f64; 3] = [0.3, 0.5, 0.7];
const NBTN: usize = 8;
/// The inventory: three rows of the main inventory, then the hotbar.
const INV_X: f32 = (W - 9.0 * SLOT) / 2.0;
const INV_Y: f32 = 152.0;
/// The search over the right side.
const SEARCH: [f32; 4] = [146.0, 18.0, 150.0, 226.0];
const ROW: f32 = 18.0;
const ROWS: usize = 11;

struct Spin {
    start: Instant,
    from: f32,
    to: f32,
    win: bool,
    chance: f32,
    /// The stake and the reward as they were when it began.
    stake: (String, u32),
    reward: (String, u32),
    ticks: i32,
    over: bool,
}

#[derive(Default)]
struct Client {
    table: Table,
    /// The table's parts come before `open`.
    incoming: Vec<Price>,
    /// The staked slot (0..36), its item and how many of it.
    bet: Option<(usize, String, u32)>,
    target: Option<(String, u32)>,
    /// The button (x2.. or 30 %..) the reward was picked with; it is picked again for a new stake.
    mode: Option<usize>,
    search: Option<(String, usize)>,
    /// An inventory slot pressed and not yet let go (slot, mouse button): dropped on the stake panel it is staked.
    drag: Option<(usize, i32)>,
    spin: Option<Spin>,
    /// Where the needle rests, in turns.
    rot: f32,
    outcome: Option<(bool, String)>,
    hint: Option<(String, Instant)>,
    waiting: Option<Instant>,
    outbox: Vec<Vec<u8>>,
    sounds: Vec<(&'static str, f32, f32)>,
}

static CLIENT: Mutex<Option<Client>> = Mutex::new(None);

fn mine(game: &Game<'_>, screen: &NativeScreenView<'_>) -> bool {
    screen.id == "mod" && game.native_mod_screen() == Some(MOD)
}

fn origin(w: f32, h: f32) -> (f32, f32) {
    (((w - W) / 2.0).floor(), ((h - H) / 2.0).floor().max(0.0))
}

fn gui_origin(game: &Game<'_>) -> Option<(f32, f32)> {
    let r = game.renderer.as_ref()?;
    let b = HudBatch::new([r.config.width, r.config.height]);
    Some(origin(b.width, b.height))
}

fn inside(p: [f32; 2], r: [f32; 4]) -> bool {
    p[0] >= r[0] && p[0] < r[0] + r[2] && p[1] >= r[1] && p[1] < r[1] + r[3]
}

fn slot_rect(i: usize) -> [f32; 4] {
    let (col, y) = if i < 9 { (i, INV_Y + 3.0 * SLOT + 4.0) } else { ((i - 9) % 9, INV_Y + ((i - 9) / 9) as f32 * SLOT) };
    [INV_X + col as f32 * SLOT, y, SLOT, SLOT]
}

fn btn_rect(i: usize) -> [f32; 4] {
    [96.0 + i as f32 * 25.0, 121.0, 23.0, 18.0]
}

/// The panel's item box, and its quantity widgets: minus, plus, the slider.
fn item_box(p: [f32; 4]) -> [f32; 4] {
    [p[0] + p[2] / 2.0 - 10.0, p[1] + 8.0, 20.0, 20.0]
}

fn qty_boxes(p: [f32; 4]) -> ([f32; 4], [f32; 4], [f32; 4]) {
    (
        [p[0] + 6.0, p[1] + 55.0, 14.0, 14.0],
        [p[0] + p[2] - 20.0, p[1] + 55.0, 14.0, 14.0],
        [p[0] + 8.0, p[1] + 76.0, p[2] - 16.0, 6.0],
    )
}

fn inv<'a>(game: &'a Game<'_>, i: usize) -> Option<&'a ItemStack> {
    game.inventory.get(i).and_then(Option::as_ref).filter(|s| s.count > 0 && &*s.item != "minecraft:air")
}

fn points(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

fn percent(p: f64) -> String {
    let v = p * 100.0;
    let s = if v >= 10.0 { format!("{v:.1}") } else if v >= 1.0 { format!("{v:.2}") } else { format!("{v:.3}") };
    if ru() { format!("{}%", s.replace('.', ",")) } else { format!("{s}%") }
}

impl Client {
    fn hint(&mut self, s: &str) {
        self.hint = Some((s.to_owned(), Instant::now()));
    }

    fn busy(&self) -> bool {
        self.waiting.is_some_and(|t| t.elapsed() < Duration::from_secs(3)) || self.spin.as_ref().is_some_and(|s| !s.over)
    }

    /// The stake's points, when it is a priced item still in its slot.
    fn bet_value(&self, game: &Game<'_>) -> Option<u64> {
        let (slot, id, n) = self.bet.as_ref()?;
        let s = inv(game, *slot).filter(|s| *s.item == **id)?;
        Some(self.table.price(id)?.pts * u64::from((*n).min(s.count)))
    }

    fn target_value(&self) -> Option<u64> {
        let (id, n) = self.target.as_ref()?;
        Some(self.table.price(id)?.pts * u64::from(*n))
    }

    fn mult_now(&self, game: &Game<'_>) -> Option<f64> {
        Some(self.target_value()? as f64 / self.bet_value(game)? as f64)
    }

    /// How many of the stake or the reward there can be at most.
    fn max_of(&self, game: &Game<'_>, is_bet: bool) -> u32 {
        if is_bet {
            self.bet.as_ref().and_then(|(slot, id, _)| inv(game, *slot).filter(|s| *s.item == **id)).map_or(1, |s| s.count)
        } else {
            self.target.as_ref().map_or(1, |(id, _)| game.native_max_stack_size(id).unwrap_or(64).max(1) * 36)
        }
    }

    fn qty_of(&self, is_bet: bool) -> Option<u32> {
        if is_bet { self.bet.as_ref().map(|b| b.2) } else { self.target.as_ref().map(|t| t.1) }
    }

    fn set_qty(&mut self, game: &Game<'_>, is_bet: bool, n: i64) {
        let n = n.clamp(1, i64::from(self.max_of(game, is_bet))) as u32;
        if is_bet {
            if let Some(b) = &mut self.bet {
                b.2 = n;
            }
            self.repick(game);
        } else {
            if let Some(t) = &mut self.target {
                t.1 = n;
            }
            self.mode = None;
        }
        self.outcome = None;
    }

    /// The button `m`'s reward: the table's item (and count) whose points come nearest the stake's times the
    /// multiplier (x`n`, or the chance's `edge / p`); within 2 %, the fewest of the dearest.
    fn pick_btn(&mut self, game: &Game<'_>, m: usize) {
        let Some(v) = self.bet_value(game) else {
            return self.hint(tr("Сначала поставь предмет из инвентаря", "Stake an item from your inventory first"));
        };
        let n = if m < MULTS.len() { f64::from(MULTS[m]) } else { self.table.edge / CHANCES[m - MULTS.len()] };
        let want = v as f64 * n;
        let mut best: Option<(f64, u32, &Price)> = None;
        for p in &self.table.prices {
            let max = game.native_max_stack_size(&p.id).unwrap_or(64).max(1);
            let c = (want / p.pts as f64).round().clamp(1.0, f64::from(max)) as u32;
            let got = (p.pts * u64::from(c)) as f64;
            if got / (v as f64) < self.table.min_mult {
                continue;
            }
            let err = ((got - want) / want).abs();
            let better = match best {
                None => true,
                Some((e, bc, _)) => {
                    let (a, b) = (err <= 0.02, e <= 0.02);
                    if a && b { c < bc || (c == bc && err < e) } else { err < e }
                }
            };
            if better {
                best = Some((err, c, p));
            }
        }
        match best.map(|(_, c, p)| (c, p.id.clone())) {
            Some((c, id)) => {
                self.target = Some((id, c));
                self.mode = Some(m);
                self.outcome = None;
            }
            None => self.hint(tr("Нет подходящей награды", "No fitting reward")),
        }
    }

    fn repick(&mut self, game: &Game<'_>) {
        if let Some(m) = self.mode {
            self.pick_btn(game, m);
        }
    }

    fn stake(&mut self, game: &Game<'_>, slot: usize, one: bool) {
        let Some(s) = inv(game, slot) else { return };
        if self.table.price(&s.item).is_none() {
            return self.hint(tr("У этого предмета нет цены — его нельзя поставить", "This item has no price: it can't be staked"));
        }
        if s.damage != 0 || s.extra.is_some() {
            return self.hint(tr("Только обычные предметы: без чар, имён и износа", "Plain items only: no enchantments, names or wear"));
        }
        let n = if one { 1 } else { s.count };
        self.bet = Some((slot, s.item.to_string(), n));
        self.outcome = None;
        self.repick(game);
    }

    fn ask(&mut self, game: &Game<'_>) {
        if self.busy() {
            return;
        }
        let (Some((slot, bet, n)), Some((target, tn))) = (self.bet.clone(), self.target.clone()) else {
            return self.hint(tr("Нужны ставка и награда", "Pick a stake and a reward"));
        };
        let Some(s) = inv(game, slot).filter(|s| *s.item == *bet) else {
            self.bet = None;
            return self.hint(tr("Ставка изменилась", "The stake changed"));
        };
        let n = n.min(s.count);
        let Some(m) = self.mult_now(game) else { return };
        if m < self.table.min_mult {
            return self.hint(tr("Награда должна быть дороже ставки", "The reward must be worth more than the stake"));
        }
        if m > self.table.max_mult {
            return self.hint(&if ru() {
                format!("Слишком большой множитель: не больше x{}", self.table.max_mult)
            } else {
                format!("Multiplier too high: x{} at most", self.table.max_mult)
            });
        }
        self.outbox.push(encode_ask(&Ask { slot, bet_count: n, target_count: tn, bet, target }));
        self.waiting = Some(Instant::now());
        self.outcome = None;
    }

    fn matches(&self, query: &str) -> Vec<&Price> {
        let q = query.to_lowercase();
        self.table
            .prices
            .iter()
            .filter(|p| q.is_empty() || p.name.to_lowercase().contains(&q) || p.id.contains(&q) || english(&p.id).to_lowercase().contains(&q))
            .collect()
    }

    /// A reward picked in the search: as few as make it dearer than the stake.
    fn choose(&mut self, game: &Game<'_>, id: &str) {
        let Some(p) = self.table.price(id) else { return };
        let max = game.native_max_stack_size(id).unwrap_or(64).max(1);
        let c = match self.bet_value(game) {
            Some(v) => ((v as f64 * self.table.min_mult / p.pts as f64).ceil() as u32).clamp(1, max * 36),
            None => 1,
        };
        self.target = Some((p.id.clone(), c));
        self.mode = None;
        self.search = None;
        self.outcome = None;
    }
}

// --- drawing ---------------------------------------------------------------------------------------

const PANEL_BG: u32 = 0xFF121823;
const PANEL_EDGE: u32 = 0xFF3A4558;
const CELL: u32 = 0xFF0C1018;
const CELL_EDGE: u32 = 0xFF2A3342;
const GOLD: u32 = 0xFFFFD54F;

fn fill(game: &Game<'_>, b: &mut HudBatch, r: [f32; 4], argb: u32) {
    game.native_fill(b, r, argb);
}

fn frame(game: &Game<'_>, b: &mut HudBatch, r: [f32; 4], face: u32, edge: u32) {
    fill(game, b, r, edge);
    fill(game, b, [r[0] + 1.0, r[1] + 1.0, r[2] - 2.0, r[3] - 2.0], face);
}

fn text(game: &Game<'_>, b: &mut HudBatch, s: &str, x: f32, y: f32, rgb: u32) {
    game.native_text(b, s, [x, y], rgb, true);
}

fn centered(game: &Game<'_>, b: &mut HudBatch, s: &str, cx: f32, y: f32, rgb: u32) {
    let w = game.native_text_width(s);
    text(game, b, s, (cx - w / 2.0).round(), y, rgb);
}

/// Text centred and scaled `k` times about its top-centre.
fn big(game: &Game<'_>, b: &mut HudBatch, s: &str, cx: f32, y: f32, rgb: u32, k: f32) {
    let start = b.vertices.len();
    centered(game, b, s, cx, y, rgb);
    b.scale_since(start, cx, y, k, k);
}

/// An item's icon and its count, as a slot shows them.
fn icon(game: &Game<'_>, b: &mut HudBatch, id: &str, n: u32, x: f32, y: f32) {
    let _ = game.native_item(b, id, [x, y], 16.0);
    if n > 1 {
        let s = n.to_string();
        let w = game.native_text_width(&s);
        text(game, b, &s, x + 17.0 - w, y + 9.0, 0xFFFFFF);
    }
}

fn wrap(game: &Game<'_>, s: &str, w: f32) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in s.split(' ') {
        let t = if cur.is_empty() { word.to_owned() } else { format!("{cur} {word}") };
        if cur.is_empty() || game.native_text_width(&t) <= w {
            cur = t;
        } else {
            out.push(std::mem::replace(&mut cur, word.to_owned()));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn mix(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let ch = |s: u32| (a >> s & 0xFF, b >> s & 0xFF);
    let m = |s: u32| {
        let (x, y) = ch(s);
        (x as f32 + (y as f32 - x as f32) * t).round() as u32
    };
    m(24) << 24 | m(16) << 16 | m(8) << 8 | m(0)
}

/// A line of width `w` from `p0` to `p1`.
fn bar(game: &Game<'_>, b: &mut HudBatch, p0: [f32; 2], p1: [f32; 2], w: f32, argb: u32) {
    let d = [p1[0] - p0[0], p1[1] - p0[1]];
    let l = (d[0] * d[0] + d[1] * d[1]).sqrt().max(1e-3);
    let n = [-d[1] / l * w / 2.0, d[0] / l * w / 2.0];
    game.native_quad(
        b,
        [[p0[0] + n[0], p0[1] + n[1]], [p1[0] + n[0], p1[1] + n[1]], [p1[0] - n[0], p1[1] - n[1]], [p0[0] - n[0], p0[1] - n[1]]],
        argb,
    );
}

/// A faint honeycomb over a panel, as a backdrop.
fn hexes(game: &Game<'_>, b: &mut HudBatch, r: [f32; 4]) {
    let s = 7.0f32;
    let (dx, dy) = (1.5 * s, 1.732_050_8 * s);
    let ok = |p: [f32; 2]| p[0] >= r[0] + 2.0 && p[0] <= r[0] + r[2] - 2.0 && p[1] >= r[1] + 2.0 && p[1] <= r[1] + r[3] - 2.0;
    let cols = (r[2] / dx) as i32 + 2;
    let rows = (r[3] / dy) as i32 + 2;
    for i in 0..cols {
        for j in 0..rows {
            let cx = r[0] + i as f32 * dx;
            let cy = r[1] + j as f32 * dy + if i % 2 == 1 { dy / 2.0 } else { 0.0 };
            let v: Vec<[f32; 2]> = (0..6)
                .map(|k| {
                    let a = std::f32::consts::FRAC_PI_3 * k as f32;
                    [cx + s * a.cos(), cy + s * a.sin()]
                })
                .collect();
            for k in 0..6 {
                if ok(v[k]) && ok(v[(k + 1) % 6]) {
                    bar(game, b, v[k], v[(k + 1) % 6], 0.7, 0x2EA88A4A);
                }
            }
        }
    }
}

/// A piece of the ring about `c` between radii `r0` and `r1`, from turn `f0` to `f1` (clockwise from the top).
fn piece(game: &Game<'_>, b: &mut HudBatch, c: [f32; 2], f: [f32; 2], r: [f32; 2], argb: u32) {
    use std::f32::consts::TAU;
    let p = |f: f32, r: f32| [c[0] + r * (f * TAU).sin(), c[1] - r * (f * TAU).cos()];
    game.native_quad(b, [p(f[0], r[0]), p(f[0], r[1]), p(f[1], r[1]), p(f[1], r[0])], argb);
}

fn arc(game: &Game<'_>, b: &mut HudBatch, c: [f32; 2], f: [f32; 2], r: [f32; 2], colour: impl Fn(f32) -> u32) {
    let step = 1.0 / 64.0;
    let mut at = f[0];
    while at < f[1] - 1e-5 {
        let e = (at + step).min(f[1]);
        piece(game, b, c, [at, e + 0.0015], r, colour(at));
        at = e;
    }
}

/// The ring: a track, the winning arc (gold to orange) over `[0, chance)`, ticks, and the needle at `needle`.
fn ring(game: &Game<'_>, b: &mut HudBatch, c: [f32; 2], chance: f32, needle: f32) {
    use std::f32::consts::TAU;
    arc(game, b, c, [0.0, 1.0], [WI - 1.5, WR + 1.5], |_| 0xFF0A0E15);
    arc(game, b, c, [0.0, 1.0], [WI, WR], |_| 0xFF1F2733);
    let chance = chance.clamp(0.0, 1.0);
    if chance > 0.0 {
        arc(game, b, c, [0.0, chance], [WI, WR], |f| mix(0xFFFFD54F, 0xFFF28C1A, f / chance));
    }
    let spoke = |game: &Game<'_>, b: &mut HudBatch, f: f32, r0: f32, r1: f32, w: f32, argb: u32| {
        let (s, co) = ((f * TAU).sin(), (f * TAU).cos());
        bar(game, b, [c[0] + r0 * s, c[1] - r0 * co], [c[0] + r1 * s, c[1] - r1 * co], w, argb);
    };
    for k in 0..40 {
        let major = k % 5 == 0;
        spoke(game, b, k as f32 / 40.0, WR + 3.0, WR + if major { 9.0 } else { 6.0 }, 1.0, if major { 0xFF8A94A6 } else { 0xFF4A5568 });
    }
    spoke(game, b, needle, WI + 2.0, WR + 2.0, 1.6, 0xFFF4F4F4);
    // The head: a gold wedge past the ring, its point toward the centre.
    let (s, co) = ((needle * TAU).sin(), (needle * TAU).cos());
    let at = |r: f32, side: f32| [c[0] + r * s + side * co, c[1] - r * co + side * s];
    game.native_quad(b, [at(WR + 1.0, 0.0), at(WR + 9.0, -3.5), at(WR + 9.0, 3.5), at(WR + 1.0, 0.0)], GOLD);
}

fn button(game: &Game<'_>, b: &mut HudBatch, r: [f32; 4], label: &str, hot: bool, on: bool, off: bool, ink: u32) {
    let face = if off { 0xFF161C28 } else if on { 0xFF2A3A52 } else if hot { 0xFF2E394B } else { 0xFF1E2636 };
    frame(game, b, r, face, if on { GOLD } else { CELL_EDGE });
    centered(game, b, label, r[0] + r[2] / 2.0, r[1] + (r[3] - 8.0) / 2.0, if off { 0x59627A } else { ink });
}

fn ease(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

impl Spin {
    fn t(&self) -> f32 {
        self.start.elapsed().as_secs_f32() / SPIN_SECS
    }
    fn rot(&self) -> f32 {
        self.from + (self.to - self.from) * ease(self.t())
    }
}

/// One item panel: the backdrop, its item box, and under it the value and quantity widgets (or a prompt).
fn side(
    game: &Game<'_>,
    b: &mut HudBatch,
    o: (f32, f32),
    m: [f32; 2],
    p: [f32; 4],
    what: &Option<(String, u32)>,
    max: u32,
    table: &Table,
    prompt: (&str, &str),
    busy: bool,
) {
    let at = |r: [f32; 4]| [o.0 + r[0], o.1 + r[1], r[2], r[3]];
    let pr = at(p);
    frame(game, b, pr, 0xFF141B27, 0xFF2C3646);
    hexes(game, b, pr);
    let bx = at(item_box(p));
    frame(game, b, bx, if inside(m, bx) && !busy { 0xFF1C2535 } else { CELL }, CELL_EDGE);
    let cx = pr[0] + pr[2] / 2.0;
    match what {
        Some((id, n)) => {
            icon(game, b, id, 1, bx[0] + 2.0, bx[1] + 2.0);
            let v = table.price(id).map_or(0, |pc| pc.pts * u64::from(*n));
            centered(game, b, &format!("{} {}", tr("Ценность:", "Value:"), points(v)), cx, pr[1] + 33.0, 0xE7E9EE);
            centered(game, b, tr("Количество", "Amount"), cx, pr[1] + 44.0, 0xA7AFBF);
            let (mi, pl, sl) = qty_boxes(p);
            for (r, s) in [(mi, "−"), (pl, "+")] {
                let r = at(r);
                frame(game, b, r, if inside(m, r) && !busy { 0xFF2E394B } else { 0xFF1A2231 }, CELL_EDGE);
                centered(game, b, s, r[0] + r[2] / 2.0, r[1] + 3.0, 0xFFFFFF);
            }
            centered(game, b, &n.to_string(), cx, pr[1] + 58.0, GOLD);
            let sl = at(sl);
            fill(game, b, sl, 0xFF1F2733);
            let frac = if max > 1 { (*n as f32 - 1.0) / (max as f32 - 1.0) } else { 1.0 };
            fill(game, b, [sl[0], sl[1], (sl[2] * frac).max(3.0), sl[3]], GOLD);
        }
        None => {
            let mut y = pr[1] + 33.0;
            for line in wrap(game, prompt.0, pr[2] - 8.0) {
                centered(game, b, &line, cx, y, 0xE7E9EE);
                y += 10.0;
            }
            y += 3.0;
            for line in wrap(game, prompt.1, pr[2] - 8.0) {
                centered(game, b, &line, cx, y, 0x6F7A90);
                y += 10.0;
            }
        }
    }
}

/// `native_screen_render_post`: the screen.
pub fn render(game: &Game<'_>, b: &mut HudBatch, screen: &NativeScreenView<'_>) {
    if !mine(game, screen) {
        return;
    }
    let guard = lock(&CLIENT);
    let Some(c) = guard.as_ref() else { return };
    let (ox, oy) = origin(b.width, b.height);
    let m = screen.mouse;
    let at = |r: [f32; 4]| [ox + r[0], oy + r[1], r[2], r[3]];
    let hot = |r: [f32; 4]| inside(m, at(r));
    let busy = c.busy();
    // The window.
    fill(game, b, [0.0, 0.0, b.width, b.height], 0x80000000);
    frame(game, b, [ox, oy, W, H], PANEL_BG, PANEL_EDGE);
    big(game, b, tr("РУЛЕТКА", "ROULETTE"), ox + W / 2.0, oy + 5.0, 0xFFFFFF, 1.7);
    // The stake and the reward panels.
    let spinning = c.spin.as_ref().filter(|s| !s.over);
    let stake = spinning.map(|s| s.stake.clone()).or_else(|| {
        let (slot, id, n) = c.bet.as_ref()?;
        let s = inv(game, *slot).filter(|s| *s.item == **id)?;
        Some((id.clone(), (*n).min(s.count)))
    });
    let reward = spinning.map(|s| s.reward.clone()).or_else(|| c.target.clone());
    let (bmax, tmax) = (c.max_of(game, true), c.max_of(game, false));
    side(
        game,
        b,
        (ox, oy),
        m,
        LEFT,
        &stake,
        bmax,
        &c.table,
        (tr("Выбери предмет для ставки", "Pick an item to stake"), tr("Перетащи из инвентаря", "Drag it from your inventory")),
        busy,
    );
    side(
        game,
        b,
        (ox, oy),
        m,
        RIGHT,
        &reward,
        tmax,
        &c.table,
        (tr("Выбери желаемый предмет", "Pick the item you want"), tr("Нажми, чтобы открыть каталог", "Click to open the catalog")),
        busy,
    );
    // The ring, and in it the chance and what became of the spin.
    let (chance, needle) = match c.spin.as_ref() {
        Some(s) => (s.chance, if s.over { s.to.fract() } else { s.rot().fract() }),
        None => (c.mult_now(game).map_or(0.0, |x| (c.table.edge / x).min(1.0) as f32), c.rot),
    };
    let wc = [ox + WX, oy + WY];
    ring(game, b, wc, chance, needle);
    let col = if chance >= 0.5 { 0x55FF55 } else if chance >= 0.25 { 0xFFFF55 } else if chance > 0.0 { 0xFF8855 } else { 0xFF5555 };
    big(game, b, &percent(f64::from(chance)), wc[0], wc[1] - 13.0, col, 1.35);
    match &c.outcome {
        Some((true, _)) => big(game, b, tr("УСПЕХ!", "SUCCESS!"), wc[0], wc[1] + 5.0, 0x55FF55, 1.0),
        Some((false, _)) => big(game, b, tr("ПРОВАЛ", "FAILED"), wc[0], wc[1] + 5.0, 0xFF5555, 1.0),
        None => centered(game, b, tr("шанс", "chance"), wc[0], wc[1] + 5.0, 0x8A94A6),
    }
    // The buttons.
    let ready = c.mult_now(game).is_some_and(|x| x >= c.table.min_mult && x <= c.table.max_mult) && !busy;
    let sr = at(SPIN_BTN);
    let sface = if !ready { 0xFF1B2230 } else if hot(SPIN_BTN) { 0xFFE0A82A } else { 0xFFC8941A };
    frame(game, b, sr, sface, if ready { 0xFF6B4A0A } else { CELL_EDGE });
    let label = if c.waiting.is_some() { "..." } else if busy { tr("Крутится...", "Spinning...") } else { tr("Крутить", "Spin") };
    centered(game, b, label, sr[0] + sr[2] / 2.0, sr[1] + 5.0, if ready { 0xFFFFFF } else { 0x59627A });
    for i in 0..NBTN {
        let r = btn_rect(i);
        let (label, ink) = if i < MULTS.len() {
            (format!("x{}", MULTS[i]), 0xFFFFFF)
        } else {
            let p = CHANCES[i - MULTS.len()];
            (format!("{}%", (p * 100.0) as u32), [0xFF5555, 0xFFFF55, 0x55FF55][i - MULTS.len()])
        };
        button(game, b, at(r), &label, hot(r), c.mode == Some(i), busy || c.bet.is_none(), ink);
    }
    // The inventory.
    text(game, b, tr("Инвентарь", "Inventory"), ox + INV_X, oy + INV_Y - 11.0, 0xA7AFBF);
    let mut under: Option<(String, u32)> = None;
    for i in 0..36 {
        let r = slot_rect(i);
        let staked = c.bet.as_ref().is_some_and(|(s, _, _)| *s == i);
        let dragged = c.drag.is_some_and(|(s, _)| s == i);
        let face = if hot(r) { 0xFF2A3342 } else { CELL };
        frame(game, b, at(r), face, if staked { GOLD } else { CELL_EDGE });
        if let Some(s) = inv(game, i) {
            if !dragged {
                let _ = game.native_item_stack(b, s, [ox + r[0] + 1.0, oy + r[1] + 1.0], 16.0);
                if s.count > 1 {
                    let n = s.count.to_string();
                    let w = game.native_text_width(&n);
                    text(game, b, &n, ox + r[0] + 18.0 - w, oy + r[1] + 10.0, 0xFFFFFF);
                }
            }
            if hot(r) && c.search.is_none() && c.drag.is_none() {
                under = Some(match c.table.price(&s.item) {
                    Some(p) => (format!("{} — {} {}", p.shown(ru()), points(p.pts), tr("очк. за шт.", "pts each")), 0xFFE082),
                    None => (tr("Нет цены — нельзя поставить", "No price: can't be staked").into(), 0xFF7777),
                });
            }
        }
    }
    // The line under the inventory: what is under the pointer, a hint, the outcome, or what to do next.
    let line = under.or(match (&c.outcome, &c.hint) {
        (_, Some((h, at))) if at.elapsed() < Duration::from_secs(4) => Some((h.clone(), 0xFF8855)),
        (Some((true, s)), _) => Some((s.clone(), 0x55FF55)),
        (Some((false, s)), _) => Some((s.clone(), 0xFF5555)),
        _ if c.bet.is_none() => Some((tr("Перетащи предмет из инвентаря на панель слева (ПКМ — 1 шт.)", "Drag an item onto the left panel (right drag: one)").into(), 0x8A94A6)),
        _ if c.target.is_none() => Some((tr("Выбери награду: x2…x10, шанс или каталог справа", "Pick a reward: x2…x10, a chance, or the catalog").into(), 0x8A94A6)),
        _ => None,
    });
    if let Some((s, rgb)) = line {
        centered(game, b, &s, ox + W / 2.0, oy + H - 14.0, rgb);
    }
    // The search.
    if let Some((q, scroll)) = &c.search {
        let r = at(SEARCH);
        frame(game, b, r, 0xF0101620, GOLD);
        frame(game, b, [r[0] + 3.0, r[1] + 3.0, r[2] - 6.0, 12.0], 0xFF05070B, CELL_EDGE);
        let ms = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis());
        let caret = if (ms / 500) % 2 == 0 { "_" } else { "" };
        let shown = if q.is_empty() { format!("{}{caret}", tr("Поиск…", "Search…")) } else { format!("{q}{caret}") };
        text(game, b, &shown, r[0] + 6.0, r[1] + 5.0, if q.is_empty() { 0x6F7A90 } else { 0xFFFFFF });
        let list = c.matches(q);
        let v = c.bet_value(game);
        for (k, p) in list.iter().skip(*scroll).take(ROWS).enumerate() {
            let y = r[1] + 18.0 + k as f32 * ROW;
            let row = [r[0] + 2.0, y, r[2] - 4.0, ROW];
            let dear = v.is_none_or(|v| p.pts as f64 * 64.0 * 36.0 >= v as f64 * c.table.min_mult);
            if inside(m, row) {
                fill(game, b, row, 0x30FFFFFF);
            }
            let _ = game.native_item(b, &p.id, [row[0] + 1.0, y + 1.0], 16.0);
            let mut name = p.shown(ru());
            while game.native_text_width(&name) > r[2] - 62.0 && name.chars().count() > 3 {
                name.pop();
            }
            text(game, b, &name, row[0] + 20.0, y + 5.0, if dear { 0xFFFFFF } else { 0x6F7A90 });
            let pts = points(p.pts);
            let w = game.native_text_width(&pts);
            text(game, b, &pts, row[0] + row[2] - w - 2.0, y + 5.0, 0xFFE082);
        }
        if list.is_empty() {
            centered(game, b, tr("Ничего нет", "Nothing found"), r[0] + r[2] / 2.0, r[1] + 24.0, 0x6F7A90);
        }
    }
    // A stack being dragged, under the pointer.
    if let Some((slot, _)) = c.drag {
        if let Some(s) = inv(game, slot) {
            let _ = game.native_item_stack(b, s, [m[0] - 8.0, m[1] - 8.0], 16.0);
        }
    }
}

fn qty_click(c: &mut Client, game: &Game<'_>, is_bet: bool, panel: [f32; 4], m: [f32; 2], button: i32) -> bool {
    if c.qty_of(is_bet).is_none() {
        return false;
    }
    let (mi, pl, sl) = qty_boxes(panel);
    let step = if button == 1 { 10 } else { 1 };
    let n = i64::from(c.qty_of(is_bet).unwrap_or(1));
    if inside(m, mi) {
        c.set_qty(game, is_bet, n - step);
    } else if inside(m, pl) {
        c.set_qty(game, is_bet, n + step);
    } else if inside(m, [sl[0], sl[1] - 3.0, sl[2], sl[3] + 6.0]) {
        let frac = ((m[0] - sl[0]) / sl[2]).clamp(0.0, 1.0);
        let max = c.max_of(game, is_bet);
        c.set_qty(game, is_bet, 1 + (frac * (max as f32 - 1.0)).round() as i64);
    } else {
        return false;
    }
    true
}

/// `native_screen_mouse_button_pressed_pre`.
pub fn press(game: &Game<'_>, screen: &NativeScreenView<'_>, button: i32, canceled: &mut bool) {
    if !mine(game, screen) {
        return;
    }
    *canceled = true;
    let Some((ox, oy)) = gui_origin(game) else { return };
    let m = [screen.mouse[0] - ox, screen.mouse[1] - oy];
    let mut guard = lock(&CLIENT);
    let Some(c) = guard.as_mut() else { return };
    if let Some((q, scroll)) = c.search.clone() {
        if !inside(m, SEARCH) {
            c.search = None;
            return;
        }
        let k = ((m[1] - SEARCH[1] - 18.0) / ROW).floor();
        if k >= 0.0 {
            let id = c.matches(&q).get(scroll + k as usize).map(|p| p.id.clone());
            if let Some(id) = id {
                c.choose(game, &id);
            }
        }
        return;
    }
    if c.busy() {
        return;
    }
    // An inventory slot is picked up; where it is let go decides (the release).
    if let Some(i) = (0..36).find(|&i| inside(m, slot_rect(i))) {
        if inv(game, i).is_some() {
            c.drag = Some((i, button));
        }
        return;
    }
    if inside(m, item_box(LEFT)) {
        c.bet = None;
        c.outcome = None;
        return;
    }
    if qty_click(c, game, true, LEFT, m, button) || qty_click(c, game, false, RIGHT, m, button) {
        return;
    }
    if inside(m, RIGHT) {
        c.search = Some((String::new(), 0));
        return;
    }
    if let Some(i) = (0..NBTN).find(|&i| inside(m, btn_rect(i))) {
        return c.pick_btn(game, i);
    }
    if inside(m, SPIN_BTN) {
        c.ask(game);
    }
}

/// `native_screen_mouse_button_released_pre`: a stack let go over the stake panel (or on its own slot, a click)
/// becomes the stake.
pub fn release(game: &Game<'_>, screen: &NativeScreenView<'_>, canceled: &mut bool) {
    if !mine(game, screen) {
        return;
    }
    *canceled = true;
    let Some((ox, oy)) = gui_origin(game) else { return };
    let m = [screen.mouse[0] - ox, screen.mouse[1] - oy];
    let mut guard = lock(&CLIENT);
    let Some(c) = guard.as_mut() else { return };
    let Some((slot, button)) = c.drag.take() else { return };
    if c.busy() || c.search.is_some() {
        return;
    }
    if inside(m, LEFT) || inside(m, slot_rect(slot)) {
        c.stake(game, slot, button == 1);
    }
}

/// `native_screen_mouse_scrolled_pre`: the list scrolls; over a panel, its amount changes.
pub fn scroll(game: &Game<'_>, screen: &NativeScreenView<'_>, delta: [f64; 2], modifiers: u32, canceled: &mut bool) {
    if !mine(game, screen) {
        return;
    }
    *canceled = true;
    let Some((ox, oy)) = gui_origin(game) else { return };
    let m = [screen.mouse[0] - ox, screen.mouse[1] - oy];
    let step: i64 = if delta[1] > 0.0 { 1 } else if delta[1] < 0.0 { -1 } else { return };
    let big = if modifiers & 1 != 0 { 10 } else { 1 };
    let mut guard = lock(&CLIENT);
    let Some(c) = guard.as_mut() else { return };
    if let Some((q, scroll)) = c.search.clone() {
        let n = c.matches(&q).len();
        let s = (scroll as i64 - step * 3).clamp(0, n.saturating_sub(ROWS) as i64);
        c.search = Some((q, s as usize));
        return;
    }
    if c.busy() {
        return;
    }
    for (is_bet, panel) in [(true, LEFT), (false, RIGHT)] {
        if inside(m, panel) {
            if let Some(n) = c.qty_of(is_bet) {
                c.set_qty(game, is_bet, i64::from(n) + step * big);
            }
        }
    }
}

/// `native_screen_key_pressed_pre`: while the search is open, its keys are its own.
pub fn key(game: &Game<'_>, screen: &NativeScreenView<'_>, key: &str, canceled: &mut bool) {
    if !mine(game, screen) {
        return;
    }
    let mut guard = lock(&CLIENT);
    let Some(c) = guard.as_mut() else { return };
    let Some((q, _)) = c.search.clone() else { return };
    *canceled = true;
    match key {
        "key.keyboard.escape" => c.search = None,
        "key.keyboard.backspace" => {
            let mut q = q;
            q.pop();
            c.search = Some((q, 0));
        }
        "key.keyboard.enter" | "key.keyboard.keypad.enter" => {
            let first = c.matches(&q).first().map(|p| p.id.clone());
            if let Some(id) = first {
                c.choose(game, &id);
            }
        }
        _ => {}
    }
}

/// `native_screen_char_typed_pre`: typing into the search.
pub fn typed(game: &Game<'_>, screen: &NativeScreenView<'_>, ch: char, canceled: &mut bool) {
    if !mine(game, screen) {
        return;
    }
    let mut guard = lock(&CLIENT);
    let Some(c) = guard.as_mut() else { return };
    if let Some((q, scroll)) = c.search.as_mut() {
        *canceled = true;
        if !ch.is_control() && q.chars().count() < 24 {
            q.push(ch);
            *scroll = 0;
        }
    }
}

/// `native_net_from_server`: the table (open the screen) and the outcome of a spin.
pub fn net_from_server(game: &mut Game<'_>, channel: &str, data: &[u8]) {
    match channel {
        CH_TABLE => {
            let mut guard = lock(&CLIENT);
            let c = guard.get_or_insert_with(Client::default);
            decode_prices(data, &mut c.incoming);
        }
        CH_OPEN => {
            {
                let mut guard = lock(&CLIENT);
                let c = guard.get_or_insert_with(Client::default);
                let Some(t) = decode_head(data, std::mem::take(&mut c.incoming)) else { return };
                c.table = t;
                c.search = None;
                c.hint = None;
                if c.spin.as_ref().is_some_and(|s| s.over) {
                    c.spin = None;
                }
                if c.target.as_ref().is_some_and(|(id, _)| c.table.price(id).is_none()) {
                    c.target = None;
                }
            }
            let _ = game.native_mod_screen_open(MOD);
        }
        CH_RESULT => {
            let mut guard = lock(&CLIENT);
            let Some(c) = guard.as_mut() else { return };
            c.waiting = None;
            match data.first() {
                Some(1) if data.len() >= 10 => {
                    let win = data[1] == 1;
                    let roll = f32::from_le_bytes([data[2], data[3], data[4], data[5]]).clamp(0.0, 0.9999);
                    let chance = f32::from_le_bytes([data[6], data[7], data[8], data[9]]).clamp(0.0, 1.0);
                    // The pointer must stop inside the right colour, clear of the edge between them.
                    let roll = if win { roll.min(chance - 0.002).max(0.001) } else { roll.max(chance + 0.002).min(0.999) };
                    let from = c.rot;
                    let to = from.floor() + 4.0 + roll;
                    let stake = c.bet.as_ref().map(|(_, id, n)| (id.clone(), *n)).unwrap_or_default();
                    let reward = c.target.clone().unwrap_or_default();
                    c.spin = Some(Spin {
                        start: Instant::now(),
                        from,
                        to,
                        win,
                        chance,
                        stake,
                        reward,
                        ticks: (from * 20.0).floor() as i32,
                        over: false,
                    });
                    c.outcome = None;
                }
                Some(0) => {
                    let why = String::from_utf8_lossy(&data[1..]).into_owned();
                    c.hint(&why);
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// The window's tick: the clicks sent, the wheel's ticks heard, its stop.
fn client_tick(game: &mut Game<'_>) {
    let lang_ru = game.settings.language.to_lowercase().starts_with("ru");
    CLIENT_RU.store(lang_ru, std::sync::atomic::Ordering::Relaxed);
    let (out, sounds) = {
        let mut guard = lock(&CLIENT);
        let Some(c) = guard.as_mut() else { return };
        if game.native_mod_screen() != Some(MOD) {
            c.search = None;
        }
        if let Some(s) = c.spin.as_mut() {
            if !s.over {
                let k = (s.rot() * 20.0).floor() as i32;
                if k != s.ticks {
                    s.ticks = k;
                    let speed = 1.0 - ease(s.t());
                    c.sounds.push(("minecraft:block.note_block.hat", 0.5, 1.6 - 0.6 * (1.0 - speed)));
                }
                if s.t() >= 1.0 {
                    s.over = true;
                    c.rot = s.to.fract();
                    let name = |(id, n): &(String, u32)| {
                        let name = c.table.price(id).map_or_else(|| english(id), |p| p.shown(ru()));
                        format!("{n} × {name}")
                    };
                    if s.win {
                        c.outcome = Some((true, format!("{} {}", tr("Победа! Награда:", "You win! Reward:"), name(&s.reward))));
                        c.sounds.push(("minecraft:entity.player.levelup", 0.8, 1.0));
                        c.sounds.push(("minecraft:block.note_block.chime", 0.7, 1.4));
                    } else {
                        c.outcome = Some((false, format!("{} ({})", tr("Мимо: ставка сгорела", "Miss: the stake is lost"), name(&s.stake))));
                        c.sounds.push(("minecraft:block.note_block.bass", 1.0, 0.6));
                        c.sounds.push(("minecraft:entity.villager.no", 0.6, 1.0));
                    }
                    c.bet = None;
                }
            }
        }
        (std::mem::take(&mut c.outbox), std::mem::take(&mut c.sounds))
    };
    for msg in out {
        if game.native_net_send_to_server(CH_SPIN, &msg).is_err() {
            let mut guard = lock(&CLIENT);
            if let Some(c) = guard.as_mut() {
                c.waiting = None;
                c.hint(tr("Этот сервер без мода Рулетка", "This server has no Roulette mod"));
            }
        }
    }
    if !sounds.is_empty() {
        if let Some(p) = game.native_player() {
            for (s, vol, pitch) in sounds {
                let _ = game.native_play_local_sound(s, [p.x, p.y, p.z], vol, pitch, 0);
            }
        }
    }
}

// ===================================================================================================
// The default table (written to config/kotost_roulette.txt when there is none)
// ===================================================================================================

const DEFAULT_TABLE: &str = r#"# Рулетка (kotost_roulette): цена предметов в очках. Строка: id = очки ; Название
# Ставить можно только предметы из этой таблицы. Шанс = edge / множитель (x2 -> 49,8 %).
# Файл перечитывается сам, перезапуск не нужен. Предметы, которых нет в игре, пропускаются.
edge = 0.996
min_mult = 1.05
max_mult = 1000

# --- Земля и камень ---
dirt = 1 ; Земля
coarse_dirt = 1 ; Каменистая земля
grass_block = 2 ; Блок травы
podzol = 2 ; Подзол
mycelium = 6 ; Мицелий
mud = 1 ; Грязь
clay_ball = 1 ; Комок глины
clay = 4 ; Глина
sand = 1 ; Песок
red_sand = 1 ; Красный песок
gravel = 1 ; Гравий
cobblestone = 1 ; Булыжник
cobbled_deepslate = 1 ; Глубинный булыжник
stone = 2 ; Камень
deepslate = 2 ; Глубинный сланец
granite = 1 ; Гранит
diorite = 1 ; Диорит
andesite = 1 ; Андезит
tuff = 1 ; Туф
calcite = 2 ; Кальцит
dripstone_block = 2 ; Блок капельника
pointed_dripstone = 1 ; Капельник
sandstone = 4 ; Песчаник
red_sandstone = 4 ; Красный песчаник
smooth_stone = 3 ; Гладкий камень
stone_bricks = 2 ; Каменные кирпичи
mossy_cobblestone = 3 ; Замшелый булыжник
moss_block = 2 ; Блок мха
netherrack = 1 ; Незерак
soul_sand = 2 ; Песок душ
soul_soil = 2 ; Почва душ
basalt = 1 ; Базальт
blackstone = 1 ; Чернокамень
magma_block = 4 ; Магмовый блок
glowstone_dust = 3 ; Светопыль
glowstone = 12 ; Светокамень
end_stone = 2 ; Эндерняк
obsidian = 12 ; Обсидиан
crying_obsidian = 20 ; Плачущий обсидиан
ice = 2 ; Лёд
packed_ice = 6 ; Плотный лёд
blue_ice = 20 ; Синий лёд
snowball = 1 ; Снежок
snow_block = 2 ; Снежный блок
prismarine_shard = 5 ; Осколок призмарина
prismarine_crystals = 6 ; Кристаллы призмарина
prismarine = 20 ; Призмарин
sea_lantern = 50 ; Морской фонарь
sponge = 60 ; Губка
glass = 2 ; Стекло
terracotta = 2 ; Терракота
bricks = 6 ; Кирпичи
brick = 1 ; Кирпич

# --- Дерево ---
oak_log = 4 ; Дубовое бревно
spruce_log = 4 ; Еловое бревно
birch_log = 4 ; Берёзовое бревно
jungle_log = 4 ; Бревно тропического дерева
acacia_log = 4 ; Акациевое бревно
dark_oak_log = 4 ; Бревно тёмного дуба
mangrove_log = 4 ; Мангровое бревно
cherry_log = 5 ; Вишнёвое бревно
pale_oak_log = 5 ; Бревно бледного дуба
bamboo = 1 ; Бамбук
crimson_stem = 4 ; Багровый стебель
warped_stem = 4 ; Искажённый стебель
oak_planks = 1 ; Дубовые доски
spruce_planks = 1 ; Еловые доски
birch_planks = 1 ; Берёзовые доски
jungle_planks = 1 ; Доски тропического дерева
acacia_planks = 1 ; Акациевые доски
dark_oak_planks = 1 ; Доски тёмного дуба
mangrove_planks = 1 ; Мангровые доски
cherry_planks = 1 ; Вишнёвые доски
crimson_planks = 1 ; Багровые доски
warped_planks = 1 ; Искажённые доски
stick = 1 ; Палка
oak_sapling = 3 ; Саженец дуба
spruce_sapling = 3 ; Саженец ели
birch_sapling = 3 ; Саженец берёзы
jungle_sapling = 4 ; Саженец тропического дерева
acacia_sapling = 3 ; Саженец акации
dark_oak_sapling = 4 ; Саженец тёмного дуба
cherry_sapling = 6 ; Саженец вишни
oak_leaves = 1 ; Дубовая листва
chest = 8 ; Сундук
crafting_table = 4 ; Верстак
barrel = 8 ; Бочка
ladder = 2 ; Лестница
bowl = 1 ; Миска
torch = 2 ; Факел
lantern = 12 ; Фонарь
campfire = 10 ; Костёр

# --- Цветы и растения ---
dandelion = 8 ; Одуванчик
poppy = 8 ; Мак
blue_orchid = 10 ; Синяя орхидея
allium = 10 ; Лук-слизун
azure_bluet = 8 ; Хаустония серая
red_tulip = 8 ; Красный тюльпан
orange_tulip = 8 ; Оранжевый тюльпан
white_tulip = 8 ; Белый тюльпан
pink_tulip = 8 ; Розовый тюльпан
oxeye_daisy = 8 ; Нивяник
cornflower = 8 ; Василёк
lily_of_the_valley = 10 ; Ландыш
sunflower = 12 ; Подсолнух
lilac = 12 ; Сирень
rose_bush = 12 ; Розовый куст
peony = 12 ; Пион
torchflower = 30 ; Факелоцвет
pitcher_plant = 30 ; Кувшиночник
wither_rose = 60 ; Роза визера
pink_petals = 6 ; Розовые лепестки
lily_pad = 4 ; Кувшинка
cactus = 3 ; Кактус
sugar_cane = 2 ; Сахарный тростник
vine = 1 ; Лианы
fern = 1 ; Папоротник
short_grass = 1 ; Трава
dead_bush = 1 ; Мёртвый куст
kelp = 1 ; Ламинария
seagrass = 1 ; Морская трава
sea_pickle = 4 ; Морской огурец
brown_mushroom = 4 ; Коричневый гриб
red_mushroom = 4 ; Красный гриб
crimson_fungus = 6 ; Багровый гриб
warped_fungus = 6 ; Искажённый гриб
nether_wart = 6 ; Незерский нарост
chorus_fruit = 6 ; Плод хоруса
glow_berries = 4 ; Светящиеся ягоды
sweet_berries = 2 ; Сладкие ягоды
glow_lichen = 2 ; Светящийся лишайник
spore_blossom = 30 ; Спороцвет
azalea = 6 ; Азалия
flowering_azalea = 8 ; Цветущая азалия

# --- Еда и ферма ---
wheat_seeds = 1 ; Семена пшеницы
wheat = 2 ; Пшеница
hay_block = 18 ; Сноп сена
pumpkin_seeds = 1 ; Семена тыквы
melon_seeds = 1 ; Семена арбуза
beetroot_seeds = 1 ; Семена свёклы
carrot = 2 ; Морковь
potato = 2 ; Картофель
baked_potato = 4 ; Печёный картофель
poisonous_potato = 1 ; Ядовитый картофель
beetroot = 2 ; Свёкла
apple = 4 ; Яблоко
golden_apple = 140 ; Золотое яблоко
enchanted_golden_apple = 3000 ; Зачарованное золотое яблоко
golden_carrot = 25 ; Золотая морковь
glistering_melon_slice = 20 ; Сверкающий ломтик арбуза
melon_slice = 1 ; Ломтик арбуза
melon = 8 ; Арбуз
pumpkin = 4 ; Тыква
carved_pumpkin = 5 ; Вырезанная тыква
jack_o_lantern = 7 ; Светильник Джека
bread = 6 ; Хлеб
cookie = 3 ; Печенье
cake = 30 ; Торт
pumpkin_pie = 10 ; Тыквенный пирог
sugar = 2 ; Сахар
egg = 2 ; Яйцо
milk_bucket = 14 ; Ведро молока
honey_bottle = 8 ; Бутылочка мёда
honeycomb = 8 ; Пчелиные соты
beef = 4 ; Сырая говядина
cooked_beef = 6 ; Стейк
porkchop = 4 ; Сырая свинина
cooked_porkchop = 6 ; Жареная свинина
chicken = 3 ; Сырая курица
cooked_chicken = 5 ; Жареная курица
mutton = 3 ; Сырая баранина
cooked_mutton = 5 ; Жареная баранина
rabbit = 4 ; Сырая крольчатина
cooked_rabbit = 6 ; Жареная крольчатина
rabbit_stew = 20 ; Тушёный кролик
mushroom_stew = 10 ; Грибной суп
beetroot_soup = 10 ; Борщ
cod = 3 ; Сырая треска
cooked_cod = 5 ; Жареная треска
salmon = 4 ; Сырой лосось
cooked_salmon = 6 ; Жареный лосось
tropical_fish = 6 ; Тропическая рыба
pufferfish = 6 ; Иглобрюх
dried_kelp = 1 ; Сушёная ламинария
cocoa_beans = 3 ; Какао-бобы

# --- Мобы ---
rotten_flesh = 1 ; Гнилая плоть
bone = 2 ; Кость
bone_meal = 1 ; Костная мука
string = 2 ; Нить
spider_eye = 3 ; Паучий глаз
fermented_spider_eye = 8 ; Приготовленный паучий глаз
gunpowder = 5 ; Порох
feather = 2 ; Перо
leather = 5 ; Кожа
rabbit_hide = 2 ; Кроличья шкурка
rabbit_foot = 20 ; Кроличья лапка
ink_sac = 3 ; Чернильный мешок
glow_ink_sac = 8 ; Светящийся чернильный мешок
slime_ball = 8 ; Слизь
slime_block = 72 ; Блок слизи
magma_cream = 12 ; Сгусток магмы
ender_pearl = 25 ; Жемчуг Края
ender_eye = 40 ; Око Края
blaze_rod = 25 ; Огненный стержень
blaze_powder = 12 ; Огненный порошок
ghast_tear = 60 ; Слеза гаста
phantom_membrane = 20 ; Мембрана фантома
shulker_shell = 150 ; Панцирь шалкера
turtle_scute = 60 ; Щиток черепахи
armadillo_scute = 20 ; Щиток броненосца
nautilus_shell = 80 ; Раковина наутилуса
heart_of_the_sea = 600 ; Сердце моря
breeze_rod = 40 ; Стержень вихря
wind_charge = 6 ; Заряд ветра
honey_block = 32 ; Блок мёда
white_wool = 4 ; Белая шерсть
skeleton_skull = 200 ; Череп скелета
zombie_head = 250 ; Голова зомби
creeper_head = 300 ; Голова крипера
wither_skeleton_skull = 600 ; Череп визер-скелета
dragon_breath = 120 ; Дыхание дракона
dragon_egg = 50000 ; Яйцо дракона
dragon_head = 5000 ; Голова дракона
nether_star = 8000 ; Звезда Незера
totem_of_undying = 1500 ; Тотем бессмертия
elytra = 12000 ; Элитры
trident = 3000 ; Трезубец
saddle = 150 ; Седло
name_tag = 120 ; Бирка
experience_bottle = 30 ; Пузырёк опыта
echo_shard = 400 ; Осколок эха
disc_fragment_5 = 300 ; Фрагмент пластинки
goat_horn = 250 ; Козий рог
sniffer_egg = 800 ; Яйцо нюхача

# --- Руды и слитки ---
coal = 5 ; Уголь
charcoal = 4 ; Древесный уголь
coal_block = 45 ; Угольный блок
raw_copper = 3 ; Сырая медь
copper_ingot = 4 ; Медный слиток
copper_block = 36 ; Медный блок
raw_iron = 8 ; Сырое железо
iron_nugget = 1 ; Железный самородок
iron_ingot = 10 ; Железный слиток
iron_block = 90 ; Железный блок
raw_iron_block = 72 ; Блок сырого железа
raw_gold = 12 ; Сырое золото
gold_nugget = 2 ; Золотой самородок
gold_ingot = 15 ; Золотой слиток
gold_block = 135 ; Золотой блок
raw_gold_block = 108 ; Блок сырого золота
redstone = 3 ; Редстоун
redstone_block = 27 ; Блок редстоуна
lapis_lazuli = 6 ; Лазурит
lapis_block = 54 ; Лазуритовый блок
quartz = 6 ; Кварц Незера
quartz_block = 24 ; Кварцевый блок
amethyst_shard = 6 ; Осколок аметиста
amethyst_block = 24 ; Аметистовый блок
emerald = 60 ; Изумруд
emerald_block = 540 ; Изумрудный блок
diamond = 100 ; Алмаз
diamond_block = 900 ; Алмазный блок
ancient_debris = 230 ; Древние обломки
netherite_scrap = 230 ; Незеритовый лом
netherite_ingot = 1000 ; Незеритовый слиток
netherite_block = 9000 ; Незеритовый блок
netherite_upgrade_smithing_template = 1500 ; Шаблон «Незеритовое улучшение»
flint = 2 ; Кремень
coal_ore = 6 ; Угольная руда
iron_ore = 10 ; Железная руда
gold_ore = 14 ; Золотая руда
diamond_ore = 110 ; Алмазная руда
emerald_ore = 70 ; Изумрудная руда
copper_ore = 4 ; Медная руда
redstone_ore = 12 ; Редстоуновая руда
lapis_ore = 20 ; Лазуритовая руда
nether_quartz_ore = 8 ; Кварцевая руда
nether_gold_ore = 10 ; Незерская золотая руда

# --- Инструменты, оружие, броня (только целые, без чар) ---
wooden_pickaxe = 4 ; Деревянная кирка
stone_pickaxe = 4 ; Каменная кирка
iron_pickaxe = 32 ; Железная кирка
golden_pickaxe = 46 ; Золотая кирка
diamond_pickaxe = 302 ; Алмазная кирка
netherite_pickaxe = 1400 ; Незеритовая кирка
stone_sword = 3 ; Каменный меч
iron_sword = 21 ; Железный меч
diamond_sword = 201 ; Алмазный меч
netherite_sword = 1300 ; Незеритовый меч
iron_axe = 31 ; Железный топор
diamond_axe = 302 ; Алмазный топор
netherite_axe = 1400 ; Незеритовый топор
iron_shovel = 12 ; Железная лопата
diamond_shovel = 102 ; Алмазная лопата
netherite_shovel = 1200 ; Незеритовая лопата
iron_hoe = 22 ; Железная мотыга
diamond_hoe = 202 ; Алмазная мотыга
mace = 2500 ; Булава
heavy_core = 1500 ; Тяжёлое ядро
bow = 10 ; Лук
crossbow = 25 ; Арбалет
arrow = 1 ; Стрела
spectral_arrow = 5 ; Спектральная стрела
shield = 20 ; Щит
fishing_rod = 6 ; Удочка
shears = 20 ; Ножницы
flint_and_steel = 12 ; Огниво
bucket = 30 ; Ведро
water_bucket = 31 ; Ведро воды
lava_bucket = 40 ; Ведро лавы
compass = 45 ; Компас
clock = 65 ; Часы
spyglass = 30 ; Подзорная труба
lead = 10 ; Поводок
iron_helmet = 50 ; Железный шлем
iron_chestplate = 80 ; Железный нагрудник
iron_leggings = 70 ; Железные поножи
iron_boots = 40 ; Железные ботинки
golden_helmet = 75 ; Золотой шлем
golden_chestplate = 120 ; Золотой нагрудник
diamond_helmet = 500 ; Алмазный шлем
diamond_chestplate = 800 ; Алмазный нагрудник
diamond_leggings = 700 ; Алмазные поножи
diamond_boots = 400 ; Алмазные ботинки
netherite_helmet = 1700 ; Незеритовый шлем
netherite_chestplate = 2000 ; Незеритовый нагрудник
netherite_leggings = 1900 ; Незеритовые поножи
netherite_boots = 1600 ; Незеритовые ботинки
turtle_helmet = 300 ; Черепаший панцирь
leather_helmet = 25 ; Кожаная шапка
leather_chestplate = 40 ; Кожаная куртка
iron_horse_armor = 120 ; Железная конская броня
golden_horse_armor = 150 ; Золотая конская броня
diamond_horse_armor = 600 ; Алмазная конская броня

# --- Механизмы и полезное ---
rail = 4 ; Рельсы
powered_rail = 20 ; Энергорельсы
minecart = 50 ; Вагонетка
redstone_torch = 4 ; Редстоуновый факел
repeater = 10 ; Повторитель
comparator = 20 ; Компаратор
piston = 20 ; Поршень
sticky_piston = 28 ; Липкий поршень
observer = 15 ; Наблюдатель
hopper = 60 ; Воронка
dispenser = 15 ; Раздатчик
dropper = 10 ; Выбрасыватель
tnt = 30 ; Динамит
note_block = 12 ; Нотный блок
jukebox = 120 ; Проигрыватель
furnace = 8 ; Печь
blast_furnace = 60 ; Плавильная печь
smoker = 15 ; Коптильня
anvil = 300 ; Наковальня
enchanting_table = 450 ; Стол зачарований
bookshelf = 20 ; Книжная полка
book = 6 ; Книга
paper = 1 ; Бумага
map = 50 ; Карта
writable_book = 10 ; Книга и перо
brewing_stand = 40 ; Варочная стойка
cauldron = 70 ; Котёл
glass_bottle = 1 ; Стеклянная бутылка
beacon = 9000 ; Маяк
conduit = 5000 ; Морской источник
respawn_anchor = 200 ; Якорь возрождения
lodestone = 1100 ; Магнетит
ender_chest = 150 ; Эндер-сундук
shulker_box = 320 ; Шалкеровый ящик
end_crystal = 300 ; Кристалл Края
end_rod = 30 ; Стержень Края
item_frame = 6 ; Рамка
painting = 6 ; Картина
armor_stand = 10 ; Стойка для брони
white_bed = 16 ; Белая кровать
white_banner = 26 ; Белый флаг
bell = 400 ; Колокол
scaffolding = 2 ; Строительные леса
crafter = 120 ; Автокрафтер
copper_bulb = 30 ; Медная лампа
lightning_rod = 12 ; Громоотвод
target = 8 ; Мишень
daylight_detector = 30 ; Датчик дневного света
redstone_lamp = 20 ; Редстоуновая лампа
recovery_compass = 2500 ; Компас восстановления
trial_key = 150 ; Ключ испытаний
ominous_trial_key = 500 ; Зловещий ключ испытаний

# --- Красители ---
white_dye = 2 ; Белый краситель
black_dye = 3 ; Чёрный краситель
red_dye = 3 ; Красный краситель
yellow_dye = 3 ; Жёлтый краситель
blue_dye = 6 ; Синий краситель
green_dye = 4 ; Зелёный краситель
orange_dye = 3 ; Оранжевый краситель
pink_dye = 3 ; Розовый краситель
purple_dye = 5 ; Фиолетовый краситель
cyan_dye = 5 ; Бирюзовый краситель
light_blue_dye = 4 ; Голубой краситель
lime_dye = 4 ; Лаймовый краситель
magenta_dye = 4 ; Пурпурный краситель
brown_dye = 3 ; Коричневый краситель
gray_dye = 3 ; Серый краситель
light_gray_dye = 3 ; Светло-серый краситель

# --- Пластинки ---
music_disc_13 = 500 ; Пластинка «13»
music_disc_cat = 500 ; Пластинка «cat»
music_disc_blocks = 500 ; Пластинка «blocks»
music_disc_chirp = 500 ; Пластинка «chirp»
music_disc_far = 500 ; Пластинка «far»
music_disc_mall = 500 ; Пластинка «mall»
music_disc_mellohi = 500 ; Пластинка «mellohi»
music_disc_stal = 500 ; Пластинка «stal»
music_disc_strad = 500 ; Пластинка «strad»
music_disc_ward = 500 ; Пластинка «ward»
music_disc_11 = 600 ; Пластинка «11»
music_disc_wait = 500 ; Пластинка «wait»
music_disc_pigstep = 1500 ; Пластинка «Pigstep»
music_disc_otherside = 1500 ; Пластинка «otherside»
music_disc_5 = 2000 ; Пластинка «5»
music_disc_relic = 1500 ; Пластинка «Relic»
"#;

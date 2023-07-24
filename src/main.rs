use std::sync::{Mutex, Arc};
use std::{net::TcpListener, collections::{HashSet, HashMap}};
use rand::Rng;
use tungstenite::Message::{Text, Close};
use serde::{Deserialize, Serialize};

type IDsize = u64;
type Cardsize = u8;
type Bidsize = u8;
type Pointsize = u8;

static DEBUG: bool = true;
static RED_CARD: Cardsize = 0;

#[derive(Deserialize)]
#[serde(tag="type")]
enum ClientMessage {
	Join { name: String },
	Rejoin { id: IDsize },
	Quit,
	// This will one day not be necessary, will use tokio/tokio-tungstenite
	// to broadcast messages instead of waiting for a ping to ask for messages
	GetState,
	StartGame,
	PlaceCard { card: Cardsize },
	Bid { bid: Bidsize },
	Pass,
	Pickup { player_index: usize }
}

#[derive(Serialize)]
#[serde(tag="type")]
enum ClientResponse<'a>{
	// Eventually InvalidRequests will be more descriptive
	InvalidRequest,
	InvalidJSON,
	NotJoined,
	ValidJoin { id: IDsize },
	ValidQuit,
	NameTaken,
	ValidRejoin { id: IDsize, is_host: bool },
	State {
		player_state: &'a PlayerState,
		game_stage: &'a GameStage,
		current_player_index: usize,
		players_num_cards: Vec<Cardsize>
	},
	GameStarted,
	ValidMove,
	RedCard,
	BlackCard,
	WonGame
}

#[derive(Serialize)]
struct PlayerState {
	name: String,
	owned: Vec<Cardsize>,
	placed: Vec<Cardsize>,
	bid: Option<Bidsize>,
	points: Pointsize,
	passed: bool
}

#[derive(Serialize, Clone)]
enum GameStage {
	Lobby { winner: Option<IDsize> },
	InitialCard,
	Turns,
	Bidding { highest_bidder: IDsize, highest_bid: Bidsize },
	Flipping { flipper: IDsize, required: Bidsize, flipped: Vec<Cardsize> },
}

struct GameState {
	unique_names: HashSet<String>,
	players: HashMap<IDsize, PlayerState>,
	stage: GameStage,
	host_id: Option<IDsize>,
	id_list: Vec<IDsize>,
	current_player_index: usize
}

#[tokio::main]
async fn main () {
	let server = TcpListener::bind("127.0.0.1:9001").unwrap();
	let global_state = GameState {
		unique_names: HashSet::new(),
		players: HashMap::new(),
		stage: GameStage::Lobby { winner: None },
		host_id: None,
		id_list: Vec::new(),
		current_player_index: 0
	};

	let state_mutex = Arc::new(Mutex::new(global_state));

	for stream in server.incoming() {
		let state_clone = Arc::clone(&state_mutex);
		tokio::spawn(async move {
			let mut websocket = tungstenite::accept(stream.unwrap()).unwrap();
			let mut player_id: Option<IDsize> = None;

			println!("Connection established");

			loop {
				let result = websocket.read();

				if let Ok(Text(text)) = result {
					let state = &mut *state_clone.lock().unwrap();
					let response = handle_message(&text, player_id, state);

					match response {
						ClientResponse::ValidJoin { id } |
						ClientResponse::ValidRejoin { id, is_host: _ } => {
							player_id = Some(id);
						},
						ClientResponse::ValidQuit => {
							println!("Quit game");
							break;
						}
						_ => ()
					}

					let message = serde_json::to_string(&response).unwrap();
					websocket.write(Text(message))
						.unwrap_or_else(|_| println!("Tried to write after closing"));
					websocket.flush()
						.unwrap_or_else(|_| println!("Tried to flush after closing"));
				}
				else if let Ok(Close(_)) = result {
					println!("Connection closed");
					break;
				}
				else if let Err(_) = result {
					println!("Connection dropped");
					break;
				}
			}
		});
	}
}

fn handle_message<'a>(text: &str, maybe_id: Option<IDsize>, state: &'a mut GameState) -> ClientResponse<'a> {
	let message_result = serde_json::from_str::<ClientMessage>(text);

	if let Err(_) = message_result {
		return ClientResponse::InvalidJSON;
	}
	let message = message_result.unwrap();

	if let None = maybe_id {
		match message {
			ClientMessage::Join { name: _ } | ClientMessage::Rejoin { id: _ } => (),
			_ => { return ClientResponse::NotJoined; }
		}
	}
	let id = maybe_id.unwrap();

	//              should change this to be the cloned value, not ref to it
	match (message, &state.stage.clone()) {
		(ClientMessage::Join { name }, GameStage::Lobby { winner: _ }) => {
			if state.unique_names.contains(&name) {
				return ClientResponse::NameTaken;
			}

			let mut id: IDsize;
			if DEBUG {
				id = state.unique_names.len() as IDsize;
			}
			else {
				loop {
					id = rand::thread_rng().gen_range(IDsize::MIN..=IDsize::MAX);
					if !state.players.contains_key(&id) {
						break
					}
				}
			}

			state.unique_names.insert(name.to_string());
			state.players.insert(id, PlayerState {
				name,
				owned: vec![0, 1, 2, 3],
				placed: Vec::new(),
				bid: None,
				points: 0,
				passed: false
			});

			if state.players.len() == 1 {
				state.host_id = Some(id);
			}

			return ClientResponse::ValidJoin { id };
		},

		(ClientMessage::Rejoin { id }, _) => {
			if state.players.contains_key(&id) {
				if let Some(host_id) = state.host_id {
					if host_id == id {
						return ClientResponse::ValidRejoin { id, is_host: true }
					}
				}

				return ClientResponse::ValidRejoin { id, is_host: false };
			}
			else {
				ClientResponse::InvalidRequest
			}
		},

		(ClientMessage::Quit, GameStage::Lobby { winner: _ }) => {
			let player = state.players.get(&id).unwrap();
			state.unique_names.remove(&player.name);
			state.players.remove(&id);

			if let Some(host_id) = state.host_id {
				if id == host_id {
					state.host_id = state.players.keys().next().cloned();
				}
			}

			ClientResponse::ValidQuit
		}

		(ClientMessage::GetState, _) => {
			let maybe_player = state.players.get(&id);
			
			if let Some(player_state) = maybe_player {
				ClientResponse::State {
					player_state: player_state,
					game_stage: &state.stage,
					current_player_index: state.current_player_index,
					players_num_cards: state.id_list
						.iter()
						.map(|id| state.players[id].placed.len() as Cardsize)
						.collect()
				}
			}
			else {
				ClientResponse::InvalidRequest
			}
		},

		(ClientMessage::StartGame, GameStage::Lobby { winner: _ }) => {
			if let Some(host_id) = state.host_id {
				if host_id == id {
					start_game(state);
					return ClientResponse::GameStarted;
				}
			}

			ClientResponse::InvalidRequest
		},

		(ClientMessage::PlaceCard { card }, GameStage::InitialCard) => {
			if card > 3 {
				return ClientResponse::InvalidRequest;
			}

			let maybe_player = state.players.get_mut(&id);

			if let Some(player) = maybe_player {
				if player.placed.len() == 0 {
					player.placed.push(card);
					if state.players.iter().all(|(_, player)| player.placed.len() == 1) {
						state.stage = GameStage::Turns;
					}
					return ClientResponse::ValidMove;
				}
			}

			ClientResponse::InvalidRequest
		},

		(ClientMessage::PlaceCard { card }, GameStage::Turns) => {
			let current_id = state.id_list[state.current_player_index];
			if card > 3 || id != current_id {
				return ClientResponse::InvalidRequest;
			}

			let player = state.players.get_mut(&id).unwrap();
			player.placed.push(card);
			increment_turn(state);

			ClientResponse::ValidMove
		},

		(ClientMessage::Bid { bid }, GameStage::Turns) => {
			let current_id = state.id_list[state.current_player_index];
			if id == current_id {
				state.stage = GameStage::Bidding { highest_bidder: id, highest_bid: bid };
				return ClientResponse::ValidMove;
			}

			ClientResponse::InvalidRequest
		}

		(ClientMessage::Bid { bid }, GameStage::Bidding { highest_bidder: _, highest_bid }) => {
			if bid > *highest_bid {
				state.stage = GameStage::Bidding { highest_bidder: id, highest_bid: bid };
				return ClientResponse::ValidMove;
			}

			ClientResponse::InvalidRequest
		}

		(ClientMessage::Pass, GameStage::Bidding { highest_bidder, highest_bid }) => {
			let maybe_player = state.players.get_mut(&id);
			if let Some(player) = maybe_player {
				player.passed = true;
				if state.players.iter().all(|(_, player)| player.passed) {
					state.stage = GameStage::Flipping {
						flipper: *highest_bidder,
						required: *highest_bid,
						flipped: Vec::new()
					}
				}

				return ClientResponse::ValidMove;
			}

			ClientResponse::InvalidRequest
		}

		(ClientMessage::Pickup { player_index }, GameStage::Flipping { flipper, required, flipped }) => {
			if player_index > state.id_list.len() {
				return ClientResponse::InvalidRequest;
			}

			let flipped_card: Cardsize;
			{
				let flip_player = state.players.get_mut(&state.id_list[player_index]).unwrap();
				let maybe_card = flip_player.placed.pop();
				if let Some(card) = maybe_card {
					flipped_card = card;
				}
				else {
					return ClientResponse::InvalidRequest;
				}
			}

			let player = state.players.get_mut(&id).unwrap();
			if flipped_card == RED_CARD {
				let lost_card = rand::thread_rng().gen_range(0..player.owned.len());
				player.owned.remove(lost_card);
				start_next_round(state, flipper);
				ClientResponse::RedCard
			}
			else {
				let mut new_flipped = flipped.clone();
				new_flipped.push(flipped_card);

				if (new_flipped.len() as Bidsize) == *required {
					player.points += 1;
					if player.points == 2 {
						start_next_game(state, id);
						return ClientResponse::WonGame;
					}
					start_next_round(state, flipper);
				}
				else {
					state.stage = GameStage::Flipping {
						flipper: *flipper,
						required: *required,
						flipped: new_flipped
					};
				}

				ClientResponse::BlackCard
			}
		}

		(_, _) => ClientResponse::InvalidRequest
	}
}

fn start_game(state: &mut GameState){
	state.stage = GameStage::InitialCard;

	for (_, player) in state.players.iter_mut() {
		player.placed = Vec::new();
		player.bid = None;
		player.points = 0;
	}

	let player_list: Vec<IDsize> = state.players.keys().cloned().collect();
	state.current_player_index = rand::thread_rng().gen_range(0..player_list.len());
	state.id_list = player_list;
}

fn increment_turn(state: &mut GameState){
	state.current_player_index = (state.current_player_index + 1) % state.players.len();
}

fn start_next_round(state: &mut GameState, flipper: &IDsize){
	state.current_player_index = (state.current_player_index + 1) % state.players.len();
	let mut maybe_player_index: Option<usize> = None;
	for (index, id) in state.id_list.iter().enumerate() {
		if id == flipper {
			maybe_player_index = Some(index);
		}
	}

	state.current_player_index = maybe_player_index.unwrap();
	increment_turn(state);

	for (_, player) in state.players.iter_mut() {
		player.placed = Vec::new();
		player.bid = None;
		player.passed = false;
	}
	state.stage = GameStage::InitialCard;
}

fn start_next_game(state: &mut GameState, id: IDsize) {
	for (_, player) in state.players.iter_mut() {
		player.owned = vec![0, 1, 2, 3];
		player.placed = Vec::new();
		player.bid = None;
		player.passed = false;
	}

	state.stage = GameStage::Lobby { winner: Some(id) };
}
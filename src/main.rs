use std::net::SocketAddr;
use std::sync::{Mutex, Arc};
use std::collections::{HashSet, HashMap};
use futures::{StreamExt, TryStreamExt, future, pin_mut};
use rand::Rng;
use tokio::net::{TcpListener, TcpStream};
use tungstenite::Message::Text;
use serde::{Deserialize, Serialize};
use futures::channel::mpsc::{unbounded, UnboundedSender};
use tokio_tungstenite::tungstenite::protocol::Message;

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
	Pickup { player_index: usize },
	Debug
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
	WonGame,
	Debug(&'a GameState)
}

#[derive(Serialize)]
enum BroadcastMessage {
	None
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

#[derive(Serialize)]
struct GameState {
	unique_names: HashSet<String>,
	players: HashMap<IDsize, PlayerState>,
	stage: GameStage,
	host_id: Option<IDsize>,
	id_list: Vec<IDsize>,
	current_player_index: usize
}

type Tx = UnboundedSender<Message>;
type PeerMap = Arc<Mutex<HashMap<SocketAddr, Tx>>>;

// A large part of this function is taken from the example code in tokio-tungstenite library
// https://github.com/snapview/tokio-tungstenite/blob/ac30533aa001aea01f44d24ab82f56067c73701d/examples/server.rs
// The code is under the same MIT license as the crate
async fn handle_connection(peer_map: PeerMap, raw_stream: TcpStream, addr: SocketAddr, state: Arc<Mutex<GameState>>) {
    println!("Incoming TCP connection from: {}", addr);

    let ws_stream = tokio_tungstenite::accept_async(raw_stream)
        .await
        .expect("Error during the websocket handshake occurred");
    println!("WebSocket connection established: {}", addr);

	let id: Option<IDsize> = None;

    let (tx, rx) = unbounded();
    peer_map.lock().unwrap().insert(addr, tx);

    let (outgoing, incoming) = ws_stream.split();

	// An entire execution of each of these blocks will complete before another message is received
	// However, the other between broadcasting and receiving is not deterministic

    let broadcast_incoming = incoming.try_for_each(|msg| {
		match msg {
			Text(text) => {
				println!("Received a message from {}: {}", addr, &text);
				let peers = peer_map.lock().unwrap();

				let self_peer =
					peers.iter().filter(|(peer_addr, _)| peer_addr == &&addr).map(|(_, ws_sink)| ws_sink);

				let state_borrow = &mut *state.lock().unwrap();

				let (response_message, broadcast_message) =
					handle_message(&text, id, state_borrow);
				let response_text = serde_json::to_string(&response_message).unwrap();
				let broadcast_text = serde_json::to_string(&broadcast_message).unwrap();

				println!("Number of peers: {}", peers.len());

				for (_, sender) in peers.iter() {
					sender.unbounded_send(Text(broadcast_text.clone())).unwrap();
					println!("Broadcast a message!");
				}

				for sender in self_peer {
					sender.unbounded_send(Text(response_text.clone())).unwrap();
					println!("Sent a response!");
				}
			}
			_ => ()
		}

        future::ok(())
    });

    let receive_from_others = rx.map(Ok).forward(outgoing);

    pin_mut!(broadcast_incoming, receive_from_others);
    future::select(broadcast_incoming, receive_from_others).await;

    println!("{} disconnected", &addr);
    peer_map.lock().unwrap().remove(&addr);
}

#[tokio::main(flavor="current_thread")]
async fn main() {
    let addr = String::from("127.0.0.1:9001");

    let pm_state = PeerMap::new(Mutex::new(HashMap::new()));
	let game_state = Arc::new(Mutex::new(GameState {
		unique_names: HashSet::new(),
		players: HashMap::new(),
		stage: GameStage::Lobby { winner: None },
		host_id: None,
		id_list: Vec::new(),
		current_player_index: 0	
	}));

    let try_socket = TcpListener::bind(&addr).await;
    let listener = try_socket.expect("Failed to bind");
    println!("Listening on: {}", addr);

    while let Ok((stream, addr)) = listener.accept().await {
        tokio::spawn(handle_connection(pm_state.clone(), stream, addr, game_state.clone()));
    }
}

fn handle_message<'a>(text: &str, maybe_id: Option<IDsize>, state: &'a mut GameState)
	-> (ClientResponse<'a>, BroadcastMessage) {

	let message_result = serde_json::from_str::<ClientMessage>(text);

	if let Err(_) = message_result {
		return (ClientResponse::InvalidJSON, BroadcastMessage::None);
	}
	let message = message_result.unwrap();

	// in the future, split hande message into id-required and join-type messages
	if let None = maybe_id {
		match message {
			ClientMessage::Join { name: _ } | ClientMessage::Rejoin { id: _ } => (),
			_ => { return (ClientResponse::NotJoined, BroadcastMessage::None); }
		}
	}

	//              should change this to be the cloned value, not ref to it
	match (message, &state.stage.clone()) {
		(ClientMessage::Join { name }, GameStage::Lobby { winner: _ }) => {
			if state.unique_names.contains(&name) {
				return (ClientResponse::NameTaken, BroadcastMessage::None);
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

			return (ClientResponse::ValidJoin { id }, BroadcastMessage::None);
		},

		(ClientMessage::Rejoin { id }, _) => {
			if state.players.contains_key(&id) {
				if let Some(host_id) = state.host_id {
					if host_id == id {
						return (ClientResponse::ValidRejoin { id, is_host: true }, BroadcastMessage::None);
					}
				}

				return (ClientResponse::ValidRejoin { id, is_host: false }, BroadcastMessage::None);
			}
			else {
				(ClientResponse::InvalidRequest, BroadcastMessage::None)
			}
		},

		(ClientMessage::Quit, GameStage::Lobby { winner: _ }) => {
			let id = maybe_id.unwrap();

			let player = state.players.get(&id).unwrap();
			state.unique_names.remove(&player.name);
			state.players.remove(&id);

			if let Some(host_id) = state.host_id {
				if id == host_id {
					state.host_id = state.players.keys().next().cloned();
				}
			}

			(ClientResponse::ValidQuit, BroadcastMessage::None)
		}

		(ClientMessage::GetState, _) => {
			let id = maybe_id.unwrap();

			let maybe_player = state.players.get(&id);
			
			if let Some(player_state) = maybe_player {
				(
					ClientResponse::State {
						player_state: player_state,
						game_stage: &state.stage,
						current_player_index: state.current_player_index,
						players_num_cards: state.id_list
							.iter()
							.map(|id| state.players[id].placed.len() as Cardsize)
							.collect()
					},
					BroadcastMessage::None
				)
			}
			else {
				(ClientResponse::InvalidRequest, BroadcastMessage::None)
			}
		},

		(ClientMessage::StartGame, GameStage::Lobby { winner: _ }) => {
			let id = maybe_id.unwrap();

			if let Some(host_id) = state.host_id {
				if host_id == id {
					start_game(state);
					return (ClientResponse::GameStarted, BroadcastMessage::None);
				}
			}

			(ClientResponse::InvalidRequest, BroadcastMessage::None)
		},

		(ClientMessage::PlaceCard { card }, GameStage::InitialCard) => {
			let id = maybe_id.unwrap();

			if card > 3 {
				return (ClientResponse::InvalidRequest, BroadcastMessage::None);
			}

			let maybe_player = state.players.get_mut(&id);

			if let Some(player) = maybe_player {
				if player.placed.len() == 0 {
					player.placed.push(card);
					if state.players.iter().all(|(_, player)| player.placed.len() == 1) {
						state.stage = GameStage::Turns;
					}
					return (ClientResponse::ValidMove, BroadcastMessage::None);
				}
			}

			(ClientResponse::InvalidRequest, BroadcastMessage::None)
		},

		(ClientMessage::PlaceCard { card }, GameStage::Turns) => {
			let id = maybe_id.unwrap();

			let current_id = state.id_list[state.current_player_index];
			if card > 3 || id != current_id {
				return (ClientResponse::InvalidRequest, BroadcastMessage::None);
			}

			let player = state.players.get_mut(&id).unwrap();
			player.placed.push(card);
			increment_turn(state);

			(ClientResponse::ValidMove, BroadcastMessage::None)
		},

		(ClientMessage::Bid { bid }, GameStage::Turns) => {
			let id = maybe_id.unwrap();

			let current_id = state.id_list[state.current_player_index];
			if id == current_id {
				state.stage = GameStage::Bidding { highest_bidder: id, highest_bid: bid };
				return (ClientResponse::ValidMove, BroadcastMessage::None);
			}

			(ClientResponse::InvalidRequest, BroadcastMessage::None)
		}

		(ClientMessage::Bid { bid }, GameStage::Bidding { highest_bidder: _, highest_bid }) => {
			let id = maybe_id.unwrap();

			if bid > *highest_bid {
				state.stage = GameStage::Bidding { highest_bidder: id, highest_bid: bid };
				return (ClientResponse::ValidMove, BroadcastMessage::None);
			}

			(ClientResponse::InvalidRequest, BroadcastMessage::None)
		}

		(ClientMessage::Pass, GameStage::Bidding { highest_bidder, highest_bid }) => {
			let id = maybe_id.unwrap();

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

				return (ClientResponse::ValidMove, BroadcastMessage::None);
			}

			(ClientResponse::InvalidRequest, BroadcastMessage::None)
		}

		(ClientMessage::Pickup { player_index }, GameStage::Flipping { flipper, required, flipped }) => {
			let id = maybe_id.unwrap();

			if player_index > state.id_list.len() {
				return (ClientResponse::InvalidRequest, BroadcastMessage::None);
			}

			let flipped_card: Cardsize;
			{
				let flip_player = state.players.get_mut(&state.id_list[player_index]).unwrap();
				let maybe_card = flip_player.placed.pop();
				if let Some(card) = maybe_card {
					flipped_card = card;
				}
				else {
					return (ClientResponse::InvalidRequest, BroadcastMessage::None);
				}
			}

			let player = state.players.get_mut(&id).unwrap();
			if flipped_card == RED_CARD {
				let lost_card = rand::thread_rng().gen_range(0..player.owned.len());
				player.owned.remove(lost_card);
				start_next_round(state, flipper);
				(ClientResponse::RedCard, BroadcastMessage::None)
			}
			else {
				let mut new_flipped = flipped.clone();
				new_flipped.push(flipped_card);

				if (new_flipped.len() as Bidsize) == *required {
					player.points += 1;
					if player.points == 2 {
						start_next_game(state, id);
						return (ClientResponse::WonGame, BroadcastMessage::None);
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

				(ClientResponse::BlackCard, BroadcastMessage::None)
			}
		}

		(ClientMessage::Debug, _) => {
			if DEBUG {
				(ClientResponse::Debug(state), BroadcastMessage::None)
			}
			else {
				(ClientResponse::InvalidRequest, BroadcastMessage::None)
			}
		}

		(_, _) => (ClientResponse::InvalidRequest, BroadcastMessage::None)
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
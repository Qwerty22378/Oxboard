//! State → `GraphSpec`: what the engine should run.
//!
//! Pure apart from [`Continuity`], the compiler's memory between graphs: which id each node,
//! edge and meter got last time (so an unchanged one keeps it, and the engine keeps it
//! running), and which output clocks the mix (so it doesn't hop when outputs come and go).
//!
//! The three interfaces are three ways of filling the same edge set:
//! - **Interface 1**: the system input → every selected output.
//! - **Interface 2**: each routed app's driver endpoint → its outputs.
//! - **Interface 3**: the matrix, source → sink at a gain. Where it names an edge the others
//!   made too, the matrix's gain wins (one edge per pair).
//!
//! Only what exists goes in: present devices, enabled endpoints, apps that have an endpoint.
//! Everything else stays in desired state and comes back when it does.
//!
//! **Levels.** Every edge into an output has a param, valued at its gain × the master level,
//! so moving any fader (master included) is a param change, never a rebuild. App levels are
//! not in the graph at all: an app's Windows session volume is its level (C4), and matrix
//! gains are sends relative to it (Interface 3 "option A"). Clips → the clip bus carry each
//! clip's own gain.

use crate::state::AppState;
use sb_shared::events::{MeterFrame, MeterLevels};
use sb_shared::graph::{EdgeSpec, GraphSpec, MeterId, MeterSpec, NodeId, NodeKind, NodeSpec, ParamId, ParamSpec};
use sb_shared::ui::{ClipId, DeviceKey, Flow, MeterTarget, Sink, Source, SystemInput};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Param and meter table sizes; the host's `HostConfig` defaults.
pub const PARAMS: u32 = 4096;
pub const METERS: u32 = 256;
/// Voices per clip (for `Retrigger::Overlap`).
pub const CLIP_POLYPHONY: u8 = 4;

/// What a node is, in the controller's terms.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum NodeKey {
    Input(DeviceKey),
    /// A driver render endpoint, read as a source.
    Endpoint(u32),
    Clip(ClipId),
    /// Every clip, mixed; what `Source::Clips` routes.
    ClipBus,
    Output(DeviceKey),
}

type EdgeKey = (NodeKey, NodeKey);

/// Ids handed out by key; freed ids are reused oldest first, so a slot isn't reused by a
/// different thing right after it was let go.
#[derive(Debug, Clone)]
struct Ids<K> {
    ids: BTreeMap<K, u32>,
    free: VecDeque<u32>,
    next: u32,
    capacity: u32,
}

impl<K: Ord + Clone> Ids<K> {
    fn new(capacity: u32) -> Self {
        Self {
            ids: BTreeMap::new(),
            free: VecDeque::new(),
            next: 0,
            capacity,
        }
    }

    fn get(&mut self, k: &K) -> Option<u32> {
        if let Some(id) = self.ids.get(k) {
            return Some(*id);
        }
        let id = match self.free.pop_front() {
            Some(id) => id,
            None if self.next < self.capacity => {
                self.next += 1;
                self.next - 1
            }
            None => return None,
        };
        self.ids.insert(k.clone(), id);
        Some(id)
    }

    /// Free every id whose key isn't in `used`.
    fn retain(&mut self, used: &BTreeSet<K>) {
        let free = &mut self.free;
        self.ids.retain(|k, id| {
            let keep = used.contains(k);
            if !keep {
                free.push_back(*id);
            }
            keep
        });
    }
}

/// What the compiler remembers between graphs.
#[derive(Debug, Clone)]
pub struct Continuity {
    nodes: Ids<NodeKey>,
    params: Ids<EdgeKey>,
    meters: Ids<MeterTarget>,
    master: Option<DeviceKey>,
}

impl Default for Continuity {
    fn default() -> Self {
        Self {
            nodes: Ids::new(u32::MAX),
            params: Ids::new(PARAMS),
            meters: Ids::new(METERS),
            master: None,
        }
    }
}

/// A graph and what the controller needs to talk about it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Compiled {
    pub graph: GraphSpec,
    /// What each meter in the graph reads, to forward meter frames to the UI.
    pub meters: BTreeMap<MeterId, MeterTarget>,
    /// Each clip's node, for triggers.
    pub clips: BTreeMap<ClipId, NodeId>,
    /// Each device node's device, for the engine's `DeviceLost`.
    pub devices: BTreeMap<NodeId, DeviceKey>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    /// Same graph, same values.
    None,
    /// Same graph shape; only these params moved (`set_param` each).
    Params(Vec<(ParamId, f32)>),
    /// A different graph: `apply_graph`.
    Graph,
}

impl Compiled {
    /// How to get the engine from `prev` to this.
    pub fn update_from(&self, prev: &Compiled) -> Update {
        let shape = |g: &GraphSpec| {
            let mut g = g.clone();
            g.params.iter_mut().for_each(|p| p.value = 0.0);
            g
        };
        if shape(&self.graph) != shape(&prev.graph) {
            return Update::Graph;
        }
        let moved: Vec<(ParamId, f32)> = self
            .graph
            .params
            .iter()
            .zip(&prev.graph.params)
            .filter(|(new, old)| new.value != old.value)
            .map(|(new, _)| (new.id, new.value))
            .collect();
        if moved.is_empty() {
            Update::None
        } else {
            Update::Params(moved)
        }
    }

    /// The engine's meter frame in the UI's terms (meters not in this graph are dropped).
    pub fn meters_for_ui(&self, frame: &MeterFrame) -> Vec<(MeterTarget, MeterLevels)> {
        frame
            .meters
            .iter()
            .filter_map(|(id, levels)| self.meters.get(id).map(|t| (t.clone(), *levels)))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CompileError {
    /// More edges than the engine's param table holds.
    TooManyEdges,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::TooManyEdges => write!(f, "more than {PARAMS} connections"),
        }
    }
}

pub fn compile(state: &AppState, cont: &mut Continuity) -> Result<Compiled, CompileError> {
    let (d, o) = (&state.desired, &state.observed);
    let present = |k: &DeviceKey, flow| state.usable(k, flow);
    let output = |k: &DeviceKey| present(k, Flow::Render).then(|| NodeKey::Output(k.clone()));
    let endpoint = |id: u32| {
        o.endpoints
            .get(&id)
            .is_some_and(|e| e.enabled)
            .then_some(NodeKey::Endpoint(id))
    };
    let app_node = |a| d.app_endpoints.get(a).and_then(|id| endpoint(*id));
    let system = match &d.system_input {
        Some(SystemInput::Device(k)) => present(k, Flow::Capture).then(|| NodeKey::Input(k.clone())),
        Some(SystemInput::Endpoint(id)) => endpoint(*id),
        None => None,
    };

    // Edges, by (from, to), with their gain before the master level. Later ones win.
    let mut edges: BTreeMap<EdgeKey, f32> = BTreeMap::new();
    if let Some(sys) = &system {
        for out in d.outputs.iter().filter_map(output) {
            edges.insert((sys.clone(), out), 1.0);
        }
    }
    for (app, outs) in &d.app_routes {
        if let Some(src) = app_node(app) {
            for out in outs.iter().filter_map(output) {
                edges.insert((src.clone(), out), 1.0);
            }
        }
    }
    let clips = !d.clips.is_empty();
    for ((from, Sink::Device(to)), gain) in &d.matrix {
        let src = match from {
            Source::Device(k) => present(k, Flow::Capture).then(|| NodeKey::Input(k.clone())),
            Source::App(a) => app_node(a),
            Source::Clips => clips.then_some(NodeKey::ClipBus),
        };
        if let (Some(src), Some(out)) = (src, output(to)) {
            edges.insert((src, out), *gain);
        }
    }
    for (id, clip) in &d.clips {
        edges.insert((NodeKey::Clip(*id), NodeKey::ClipBus), clip.settings.gain);
    }

    // Nodes: everything an edge touches. Clips are always there (so they can be triggered and
    // metered before they're routed anywhere).
    let mut nodes: BTreeSet<NodeKey> = edges.keys().flat_map(|(a, b)| [a.clone(), b.clone()]).collect();
    if clips {
        nodes.insert(NodeKey::ClipBus);
    }

    let mut c = Compiled::default();
    let master_gain = d.master.effective();
    let mut ids = BTreeMap::new();
    for key in &nodes {
        let id = NodeId(cont.nodes.get(key).expect("node ids don't run out"));
        ids.insert(key.clone(), id);
        let kind = match key {
            NodeKey::Input(k) => {
                c.devices.insert(id, k.clone());
                NodeKind::DeviceInput { device: k.0.clone() }
            }
            NodeKey::Endpoint(e) => NodeKind::VirtualSource { endpoint: *e },
            NodeKey::Clip(clip) => {
                let def = &d.clips[clip];
                c.clips.insert(*clip, id);
                NodeKind::Clip {
                    path: def.path.clone(),
                    polyphony: CLIP_POLYPHONY,
                    retrigger: def.settings.retrigger,
                    looped: def.settings.looped,
                }
            }
            NodeKey::ClipBus => NodeKind::Bus,
            NodeKey::Output(k) => {
                c.devices.insert(id, k.clone());
                NodeKind::DeviceOutput { device: k.0.clone() }
            }
        };
        c.graph.nodes.push(NodeSpec { id, kind });
    }
    for ((from, to), gain) in &edges {
        let param = ParamId(
            cont.params
                .get(&(from.clone(), to.clone()))
                .ok_or(CompileError::TooManyEdges)?,
        );
        let value = if matches!(to, NodeKey::Output(_)) {
            gain * master_gain
        } else {
            *gain
        };
        c.graph.edges.push(EdgeSpec {
            from: ids[from],
            to: ids[to],
            gain: Some(param),
        });
        c.graph.params.push(ParamSpec { id: param, value });
    }

    // Meters: on every node with something to show, as the UI names it.
    let mut metered = BTreeSet::new();
    for key in &nodes {
        let target = match key {
            NodeKey::Input(k) | NodeKey::Output(k) => Some(MeterTarget::Device(k.clone())),
            NodeKey::Endpoint(e) => match d.app_endpoints.iter().find(|(_, id)| *id == e) {
                Some((app, _)) => Some(MeterTarget::App(app.clone())),
                None => (system.as_ref() == Some(key)).then_some(MeterTarget::System),
            },
            NodeKey::ClipBus => Some(MeterTarget::Clips),
            NodeKey::Clip(_) => None,
        };
        // Past the table's end, a node just goes unmetered.
        if let Some(t) = target
            && let Some(m) = cont.meters.get(&t)
        {
            c.graph.meters.push(MeterSpec {
                id: MeterId(m),
                node: ids[key],
            });
            c.meters.insert(MeterId(m), t.clone());
            metered.insert(t);
        }
    }

    // The master clock: the same output as last time while it's still in the graph, else the
    // first selected output, else any output.
    let outputs: Vec<&DeviceKey> = nodes
        .iter()
        .filter_map(|k| match k {
            NodeKey::Output(d) => Some(d),
            _ => None,
        })
        .collect();
    let master = cont
        .master
        .as_ref()
        .filter(|m| outputs.contains(m))
        .or_else(|| outputs.iter().copied().find(|k| d.outputs.contains(*k)))
        .or_else(|| outputs.first().copied())
        .cloned();
    c.graph.master = master.as_ref().map(|k| ids[&NodeKey::Output(k.clone())]);
    cont.master = master;

    cont.nodes.retain(&nodes);
    cont.params.retain(&edges.keys().cloned().collect());
    cont.meters.retain(&metered);
    Ok(c)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::commands;
    use crate::state::{DeviceInfo, Loopback, Observation, PoolEndpoint};
    use sb_shared::graph::Retrigger;
    use sb_shared::ui::{AppKey, Command};

    pub fn dev(name: &str) -> DeviceKey {
        DeviceKey(format!("{{0.0.0.00000000}}.{{{name}}}"))
    }
    pub fn app(name: &str) -> AppKey {
        AppKey(format!(r"C:\Apps\{name}.exe"))
    }

    /// Speakers, headphones and a mic; VB-Cable (its input loops to its output); apps with
    /// sessions; driver endpoints 1–3 enabled.
    pub fn world() -> AppState {
        let mut s = AppState::default();
        for (name, flow) in [
            ("speakers", Flow::Render),
            ("headphones", Flow::Render),
            ("mic", Flow::Capture),
            ("cable-in", Flow::Render),
            ("cable-out", Flow::Capture),
        ] {
            s.observe(Observation::DeviceAdded {
                key: dev(name),
                info: DeviceInfo {
                    name: name.into(),
                    flow,
                },
            });
        }
        s.observe(Observation::Loopback {
            render: dev("cable-in"),
            into: Some(Loopback::Capture(dev("cable-out"))),
        });
        for (i, name) in ["discord", "game", "spotify"].into_iter().enumerate() {
            s.observe(Observation::SessionAdded {
                app: app(name),
                name: name.into(),
                pid: 100 + i as u32,
            });
        }
        for id in 1..=3 {
            s.observe(Observation::Endpoint {
                id,
                state: Some(PoolEndpoint { enabled: true }),
            });
        }
        s
    }

    fn run(s: &mut AppState, cmd: Command) {
        commands::apply(s, cmd.clone()).unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
    }

    /// The engine's own rules, with the host's table sizes.
    fn engine_check(c: &Compiled) {
        let tables = sb_engine::graph::compile::Tables::new(PARAMS as usize, METERS as usize);
        if let Err(e) = sb_engine::graph::compile::check(&c.graph, &tables) {
            panic!("the engine refuses {:#?}: {e}", c.graph);
        }
    }

    fn compiled(s: &AppState) -> Compiled {
        let c = compile(s, &mut Continuity::default()).unwrap();
        engine_check(&c);
        c
    }

    fn name(c: &Compiled, id: NodeId) -> String {
        match &c.graph.nodes.iter().find(|n| n.id == id).unwrap().kind {
            NodeKind::DeviceInput { device } | NodeKind::DeviceOutput { device } => device
                .trim_start_matches("{0.0.0.00000000}.{")
                .trim_end_matches('}')
                .into(),
            NodeKind::VirtualSource { endpoint } => format!("endpoint {endpoint}"),
            NodeKind::Clip { path, .. } => format!("clip {path}"),
            NodeKind::Bus => "clips".into(),
            other => format!("{other:?}"),
        }
    }

    /// Every edge as (from, to, param value), by name.
    fn edges(c: &Compiled) -> BTreeSet<(String, String, String)> {
        let value = |p: ParamId| c.graph.params.iter().find(|q| q.id == p).unwrap().value;
        c.graph
            .edges
            .iter()
            .map(|e| (name(c, e.from), name(c, e.to), format!("{:.2}", value(e.gain.unwrap()))))
            .collect()
    }

    fn e(from: &str, to: &str, v: f32) -> (String, String, String) {
        (from.into(), to.into(), format!("{v:.2}"))
    }

    fn master(c: &Compiled) -> Option<String> {
        c.graph.master.map(|m| name(c, m))
    }

    #[test]
    fn interface_1_plays_the_system_input_on_every_selected_output() {
        let mut s = world();
        assert!(compiled(&s).graph.nodes.is_empty(), "nothing asked for, nothing built");
        run(
            &mut s,
            Command::SetSystemInput(Some(SystemInput::Device(dev("cable-out")))),
        );
        for out in ["speakers", "headphones"] {
            run(
                &mut s,
                Command::SelectOutput {
                    device: dev(out),
                    selected: true,
                },
            );
        }
        run(&mut s, Command::SetMasterGain(0.5));
        let c = compiled(&s);
        assert_eq!(
            edges(&c),
            [e("cable-out", "speakers", 0.5), e("cable-out", "headphones", 0.5)].into()
        );
        assert!(master(&c).is_some());
        let metered: BTreeSet<_> = c.meters.values().cloned().collect();
        assert_eq!(
            metered,
            [dev("cable-out"), dev("speakers"), dev("headphones")]
                .map(MeterTarget::Device)
                .into()
        );

        // Headphones unplugged: gone from the graph, still selected.
        s.observe(Observation::DeviceRemoved { key: dev("headphones") });
        assert_eq!(edges(&compiled(&s)), [e("cable-out", "speakers", 0.5)].into());
        assert!(s.desired.outputs.contains(&dev("headphones")));
        // No system input (the cable's gone too): nothing to play.
        s.observe(Observation::DeviceRemoved { key: dev("cable-out") });
        assert!(compiled(&s).graph.nodes.is_empty());
        // With the driver: the system input is an endpoint.
        run(&mut s, Command::SetSystemInput(Some(SystemInput::Endpoint(3))));
        let c = compiled(&s);
        assert_eq!(edges(&c), [e("endpoint 3", "speakers", 0.5)].into());
        assert!(c.meters.values().any(|t| *t == MeterTarget::System));
        s.observe(Observation::Endpoint {
            id: 3,
            state: Some(PoolEndpoint { enabled: false }),
        });
        assert!(
            compiled(&s).graph.nodes.is_empty(),
            "a disabled endpoint isn't a source"
        );
    }

    #[test]
    fn interface_2_plays_each_app_from_its_own_endpoint() {
        let mut s = world();
        let d = &mut s.desired;
        d.app_endpoints.insert(app("discord"), 1);
        d.app_endpoints.insert(app("game"), 2);
        d.app_routes.insert(app("discord"), [dev("headphones")].into());
        d.app_routes
            .insert(app("game"), [dev("headphones"), dev("speakers")].into());
        d.app_routes.insert(app("spotify"), [dev("speakers")].into()); // no endpoint yet
        let c = compiled(&s);
        assert_eq!(
            edges(&c),
            [
                e("endpoint 1", "headphones", 1.0),
                e("endpoint 2", "headphones", 1.0),
                e("endpoint 2", "speakers", 1.0),
            ]
            .into()
        );
        let apps: BTreeSet<_> = c
            .meters
            .values()
            .filter(|t| matches!(t, MeterTarget::App(_)))
            .cloned()
            .collect();
        assert_eq!(
            apps,
            [MeterTarget::App(app("discord")), MeterTarget::App(app("game"))].into()
        );
        // Alongside Interface 1 (the rest of the PC on endpoint 3), master on everything.
        run(&mut s, Command::SetSystemInput(Some(SystemInput::Endpoint(3))));
        run(
            &mut s,
            Command::SelectOutput {
                device: dev("speakers"),
                selected: true,
            },
        );
        run(&mut s, Command::SetMasterGain(0.8));
        s.observe(Observation::Endpoint {
            id: 2,
            state: Some(PoolEndpoint { enabled: false }),
        });
        assert_eq!(
            edges(&compiled(&s)),
            [e("endpoint 1", "headphones", 0.8), e("endpoint 3", "speakers", 0.8)].into()
        );
    }

    #[test]
    fn interface_3_routes_the_matrix_and_mixes_clips_on_a_bus() {
        let mut s = world();
        s.desired.app_endpoints.insert(app("discord"), 1);
        s.desired.app_routes.insert(app("discord"), [dev("headphones")].into());
        for (path, gain) in [("/clips/a.ogg", 1.0), ("/clips/b.ogg", 0.5)] {
            run(
                &mut s,
                Command::AddClip {
                    path: path.into(),
                    name: String::new(),
                },
            );
            let id = ClipId(s.desired.next_clip - 1);
            let mut settings = s.desired.clips[&id].settings.clone();
            settings.gain = gain;
            run(&mut s, Command::UpdateClip { id, settings });
        }
        let route = |from, to: &str, g: f32| Command::SetRoute {
            from,
            to: Sink::Device(dev(to)),
            gain: Some(g),
        };
        // "Soundboard mic": mic + clips → the cable Discord listens to; monitor on headphones.
        run(&mut s, route(Source::Device(dev("mic")), "cable-in", 1.0));
        run(&mut s, route(Source::Clips, "cable-in", 0.7));
        run(&mut s, route(Source::Clips, "headphones", 0.3));
        // The matrix's gain wins over Interface 2's for the same pair.
        run(&mut s, route(Source::App(app("discord")), "headphones", 0.5));
        // An app without an endpoint can't be separated from the rest: not routed.
        run(&mut s, route(Source::App(app("spotify")), "speakers", 1.0));
        let c = compiled(&s);
        assert_eq!(
            edges(&c),
            [
                e("mic", "cable-in", 1.0),
                e("clips", "cable-in", 0.7),
                e("clips", "headphones", 0.3),
                e("endpoint 1", "headphones", 0.5),
                e("clip /clips/a.ogg", "clips", 1.0),
                e("clip /clips/b.ogg", "clips", 0.5),
            ]
            .into()
        );
        assert_eq!(c.clips.len(), 2);
        let clip_node = c.graph.nodes.iter().find(|n| n.id == c.clips[&ClipId(1)]).unwrap();
        assert_eq!(
            clip_node.kind,
            NodeKind::Clip {
                path: "/clips/b.ogg".into(),
                polyphony: CLIP_POLYPHONY,
                retrigger: Retrigger::Restart,
                looped: false
            }
        );
        // Mic unplugged: its edge goes, the clips stay.
        s.observe(Observation::DeviceRemoved { key: dev("mic") });
        assert!(!edges(&compiled(&s)).contains(&e("mic", "cable-in", 1.0)));
        // Unrouted clips are still built (and metered), so they can be triggered.
        for g in [Source::Clips] {
            for out in ["cable-in", "headphones"] {
                run(
                    &mut s,
                    Command::SetRoute {
                        from: g.clone(),
                        to: Sink::Device(dev(out)),
                        gain: None,
                    },
                );
            }
        }
        let c = compiled(&s);
        assert_eq!(c.clips.len(), 2);
        assert!(c.meters.values().any(|t| *t == MeterTarget::Clips));
    }

    #[test]
    fn faders_move_params_and_structure_changes_rebuild() {
        let mut s = world();
        let mut cont = Continuity::default();
        run(
            &mut s,
            Command::SetSystemInput(Some(SystemInput::Device(dev("cable-out")))),
        );
        run(
            &mut s,
            Command::SelectOutput {
                device: dev("speakers"),
                selected: true,
            },
        );
        run(
            &mut s,
            Command::AddClip {
                path: "/x.ogg".into(),
                name: String::new(),
            },
        );
        let mut prev = compile(&s, &mut cont).unwrap();
        let mut next = |s: &AppState, prev: &mut Compiled| {
            let c = compile(s, &mut cont).unwrap();
            engine_check(&c);
            let u = c.update_from(prev);
            *prev = c;
            u
        };
        assert_eq!(next(&s, &mut prev), Update::None, "same state, same graph");
        run(&mut s, Command::SetMasterGain(0.25));
        let Update::Params(p) = next(&s, &mut prev) else {
            panic!()
        };
        assert_eq!(p.len(), 1, "master: the one edge into an output");
        assert_eq!(p[0].1, 0.25);
        run(&mut s, Command::SetMasterMuted(true));
        assert!(matches!(next(&s, &mut prev), Update::Params(p) if p[0].1 == 0.0));
        let mut settings = s.desired.clips[&ClipId(0)].settings.clone();
        settings.gain = 0.6;
        run(
            &mut s,
            Command::UpdateClip {
                id: ClipId(0),
                settings: settings.clone(),
            },
        );
        assert!(matches!(next(&s, &mut prev), Update::Params(p) if p == vec![(p[0].0, 0.6)]));
        // App levels are Windows session volumes, not graph params.
        run(
            &mut s,
            Command::SetAppGain {
                app: app("game"),
                gain: 0.1,
            },
        );
        assert_eq!(next(&s, &mut prev), Update::None);
        // Shape changes: a new output, a loop setting.
        run(
            &mut s,
            Command::SelectOutput {
                device: dev("headphones"),
                selected: true,
            },
        );
        assert_eq!(next(&s, &mut prev), Update::Graph);
        settings.looped = true;
        run(
            &mut s,
            Command::UpdateClip {
                id: ClipId(0),
                settings,
            },
        );
        assert_eq!(next(&s, &mut prev), Update::Graph);
    }

    #[test]
    fn unchanged_things_keep_their_ids_and_the_master_stays_put() {
        let mut s = world();
        let mut cont = Continuity::default();
        run(
            &mut s,
            Command::SetSystemInput(Some(SystemInput::Device(dev("cable-out")))),
        );
        run(
            &mut s,
            Command::SelectOutput {
                device: dev("speakers"),
                selected: true,
            },
        );
        let a = compile(&s, &mut cont).unwrap();
        assert_eq!(master(&a).as_deref(), Some("speakers"));
        let ids = |c: &Compiled| -> BTreeMap<String, (NodeId, Option<ParamId>)> {
            c.graph
                .edges
                .iter()
                .map(|e| (format!("{}>{}", name(c, e.from), name(c, e.to)), (e.to, e.gain)))
                .collect()
        };
        // Headphones join (and sort first): the speakers keep their ids and stay the master.
        run(
            &mut s,
            Command::SelectOutput {
                device: dev("headphones"),
                selected: true,
            },
        );
        let b = compile(&s, &mut cont).unwrap();
        assert_eq!(ids(&a)["cable-out>speakers"], ids(&b)["cable-out>speakers"]);
        assert_eq!(master(&b).as_deref(), Some("speakers"));
        // Speakers unplugged: the master moves to what's left.
        s.observe(Observation::DeviceRemoved { key: dev("speakers") });
        let c = compile(&s, &mut cont).unwrap();
        assert_eq!(master(&c).as_deref(), Some("headphones"));
        // ...and stays there when the speakers come back.
        s.observe(Observation::DeviceAdded {
            key: dev("speakers"),
            info: DeviceInfo {
                name: "speakers".into(),
                flow: Flow::Render,
            },
        });
        let d = compile(&s, &mut cont).unwrap();
        assert_eq!(master(&d).as_deref(), Some("headphones"));
        // The speakers' old ids were freed, and a returning device doesn't get its freed slot
        // back right away (oldest freed first): what matters is no clash with live ids.
        let live: BTreeSet<_> = d.graph.params.iter().map(|p| p.id).collect();
        assert_eq!(live.len(), d.graph.params.len());
    }

    #[test]
    fn freed_ids_are_reused_oldest_first() {
        let mut ids: Ids<&str> = Ids::new(3);
        assert_eq!(
            (ids.get(&"a"), ids.get(&"b"), ids.get(&"c")),
            (Some(0), Some(1), Some(2))
        );
        assert_eq!(ids.get(&"d"), None, "full");
        ids.retain(&["c"].into());
        assert_eq!(ids.get(&"c"), Some(2), "kept");
        assert_eq!((ids.get(&"e"), ids.get(&"f")), (Some(0), Some(1)));
    }

    #[test]
    fn meter_frames_are_renamed_for_the_ui() {
        let mut s = world();
        run(
            &mut s,
            Command::SetSystemInput(Some(SystemInput::Device(dev("cable-out")))),
        );
        run(
            &mut s,
            Command::SelectOutput {
                device: dev("speakers"),
                selected: true,
            },
        );
        let c = compiled(&s);
        let (id, target) = c.meters.iter().next().unwrap();
        let levels = MeterLevels {
            peak: [0.5, 0.5],
            rms: [0.1, 0.1],
        };
        let frame = MeterFrame {
            meters: vec![(*id, levels), (MeterId(250), levels)],
        };
        assert_eq!(
            c.meters_for_ui(&frame),
            vec![(target.clone(), levels)],
            "unknown meter dropped"
        );
    }

    /// xorshift64*
    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) % n
        }
    }

    #[test]
    fn every_graph_from_a_random_walk_passes_the_engine_and_updates_add_up() {
        let mut r = Rng(0x5eed);
        let mut s = world();
        let mut cont = Continuity::default();
        let mut prev = Compiled::default();
        let devs = ["speakers", "headphones", "mic", "cable-in", "cable-out"];
        let apps = ["discord", "game", "spotify"];
        let (mut graphs, mut params) = (0, 0);
        for _ in 0..3000 {
            let d = dev(devs[r.below(5) as usize]);
            let a = app(apps[r.below(3) as usize]);
            let gain = r.below(21) as f32 / 10.0;
            let source = match r.below(3) {
                0 => Source::Device(d.clone()),
                1 => Source::App(a.clone()),
                _ => Source::Clips,
            };
            let out = dev(devs[r.below(5) as usize]);
            let cmd = match r.below(12) {
                0 => Command::SelectOutput {
                    device: d.clone(),
                    selected: r.below(2) == 0,
                },
                1 => Command::SetSystemInput(match r.below(3) {
                    0 => None,
                    1 => Some(SystemInput::Device(d.clone())),
                    _ => Some(SystemInput::Endpoint(1 + r.below(3) as u32)),
                }),
                2 => Command::SetMasterGain(gain),
                3 => Command::SetMasterMuted(r.below(2) == 0),
                4 | 5 => Command::SetRoute {
                    from: source,
                    to: Sink::Device(out),
                    gain: (r.below(3) > 0).then_some(gain),
                },
                6 => Command::AddClip {
                    path: format!("/clips/{}.ogg", r.below(4)),
                    name: String::new(),
                },
                7 => Command::RemoveClip {
                    id: ClipId(r.below(s.desired.next_clip as u64 + 1) as u32),
                },
                _ => {
                    // Something changes in the world instead.
                    match r.below(4) {
                        0 => s.observe(Observation::DeviceRemoved { key: d.clone() }),
                        1 => s.observe(Observation::DeviceAdded {
                            key: d.clone(),
                            info: world().observed.devices[&d].clone(),
                        }),
                        2 => s.observe(Observation::Endpoint {
                            id: 1 + r.below(3) as u32,
                            state: Some(PoolEndpoint {
                                enabled: r.below(2) == 0,
                            }),
                        }),
                        _ => {
                            let routes = &mut s.desired.app_routes;
                            routes.entry(a.clone()).or_default().insert(out);
                            s.desired.app_endpoints.insert(a, 1 + r.below(3) as u32);
                        }
                    }
                    Command::SetMasterMuted(s.desired.master.muted)
                }
            };
            let _ = commands::apply(&mut s, cmd); // refusals are fine; state stays valid
            assert_eq!(crate::loops::find(&s), None, "a command let a loop through");
            let c = compile(&s, &mut cont).unwrap();
            engine_check(&c);
            match c.update_from(&prev) {
                Update::None => assert_eq!(c, prev),
                Update::Params(moved) => {
                    // Applying the moved params to the old graph gives the new one.
                    let mut patched = prev.clone();
                    for (id, v) in moved {
                        patched.graph.params.iter_mut().find(|p| p.id == id).unwrap().value = v;
                    }
                    assert_eq!(patched.graph, c.graph);
                    params += 1;
                }
                Update::Graph => graphs += 1,
            }
            prev = c;
        }
        assert!(graphs > 100 && params > 100, "{graphs} graphs, {params} param updates");
    }
}

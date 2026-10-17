//! Feedback loops in the desired routing.
//!
//! Inside one graph, sources and sinks never mix, so the engine never sees a cycle. Loops
//! close *outside* it: something plays into a render device whose sound comes back in as a
//! source (a cable's other end, one of the driver's endpoints), and that source is routed to
//! the same render device again. The result is a howl, so commands that would close one are
//! refused, and the view marks the connections that would (the UI greys them out).
//!
//! The check runs on desired state, present or not: a route to an unplugged device closes the
//! loop the moment it comes back.

use crate::state::{AppState, Loopback};
use sb_shared::ui::{DeviceKey, Flow, Sink, Source, SystemInput};
use std::collections::{BTreeMap, BTreeSet};

/// A point audio flows through.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Point {
    Source(Source),
    /// The system input (Interface 1's source).
    System,
    /// A render device.
    Out(DeviceKey),
}

type Edges = BTreeMap<Point, BTreeSet<Point>>;

fn edges(state: &AppState) -> Edges {
    let d = &state.desired;
    let mut e = Edges::new();
    let mut add = |from: Point, to: Point| {
        e.entry(from).or_default().insert(to);
    };
    for out in &d.outputs {
        add(Point::System, Point::Out(out.clone()));
    }
    for (app, outs) in &d.app_routes {
        for out in outs {
            add(Point::Source(Source::App(app.clone())), Point::Out(out.clone()));
        }
    }
    for (from, Sink::Device(out)) in d.matrix.keys() {
        add(Point::Source(from.clone()), Point::Out(out.clone()));
    }
    // A loopback's far end is every source that reads it.
    for (render, back) in &state.observed.loopbacks {
        let out = Point::Out(render.clone());
        match back {
            Loopback::Capture(dev) => {
                add(out.clone(), Point::Source(Source::Device(dev.clone())));
                if d.system_input == Some(SystemInput::Device(dev.clone())) {
                    add(out, Point::System);
                }
            }
            Loopback::Endpoint(id) => {
                if d.system_input == Some(SystemInput::Endpoint(*id)) {
                    add(out.clone(), Point::System);
                }
                for (app, _) in d.app_endpoints.iter().filter(|(_, e)| *e == id) {
                    add(out.clone(), Point::Source(Source::App(app.clone())));
                }
            }
        }
    }
    e
}

/// Everything reachable from `from` (not including `from` itself unless on a loop).
fn reach(e: &Edges, from: &Point) -> BTreeSet<Point> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<&Point> = e.get(from).into_iter().flatten().collect();
    while let Some(p) = stack.pop() {
        if seen.insert(p.clone()) {
            stack.extend(e.get(p).into_iter().flatten());
        }
    }
    seen
}

/// A loop in the desired routing, as the points along it, if there is one.
pub fn find(state: &AppState) -> Option<Vec<Point>> {
    let e = edges(state);
    // Depth-first with an explicit path; the graph is tiny.
    fn walk(e: &Edges, p: &Point, path: &mut Vec<Point>, done: &mut BTreeSet<Point>) -> Option<Vec<Point>> {
        if let Some(i) = path.iter().position(|q| q == p) {
            return Some(path[i..].to_vec());
        }
        if done.contains(p) {
            return None;
        }
        path.push(p.clone());
        for next in e.get(p).into_iter().flatten() {
            if let Some(l) = walk(e, next, path, done) {
                return Some(l);
            }
        }
        path.pop();
        done.insert(p.clone());
        None
    }
    let mut done = BTreeSet::new();
    for p in e.keys() {
        if let Some(l) = walk(&e, p, &mut Vec::new(), &mut done) {
            return Some(l);
        }
    }
    None
}

/// What would close a loop if connected: matrix cells (source, sink), and render devices that
/// can't be Interface 1 outputs.
pub struct WouldLoop {
    pub cells: BTreeSet<(Source, Sink)>,
    pub outputs: BTreeSet<DeviceKey>,
}

/// Connecting X → out closes a loop exactly when out already reaches X.
pub fn would_loop(state: &AppState, sources: &[Source]) -> WouldLoop {
    let e = edges(state);
    let mut w = WouldLoop {
        cells: BTreeSet::new(),
        outputs: BTreeSet::new(),
    };
    let outs = state
        .observed
        .devices
        .iter()
        .chain(&state.desired.known_devices)
        .filter(|(_, i)| i.flow == Flow::Render)
        .map(|(k, _)| k);
    for out in outs {
        let r = reach(&e, &Point::Out(out.clone()));
        if r.contains(&Point::System) {
            w.outputs.insert(out.clone());
        }
        for s in sources {
            if r.contains(&Point::Source(s.clone())) {
                w.cells.insert((s.clone(), Sink::Device(out.clone())));
            }
        }
    }
    w
}

/// "Speakers → Microphone → Speakers", for an error message.
pub fn describe(state: &AppState, path: &[Point]) -> String {
    let name = |p: &Point| match p {
        Point::Source(Source::Device(k)) | Point::Out(k) => state.device(k).map_or(k.0.clone(), |d| d.name.clone()),
        Point::Source(Source::App(a)) => state.app_name(a).unwrap_or(&a.0).to_owned(),
        Point::Source(Source::Clips) => "Clips".into(),
        Point::System => match &state.desired.system_input {
            Some(SystemInput::Device(k)) => format!(
                "the PC's sound (from {})",
                state.device(k).map_or(k.0.as_str(), |d| d.name.as_str())
            ),
            Some(SystemInput::Endpoint(id)) => format!("the PC's sound (from driver endpoint {id})"),
            None => "the PC's sound".into(),
        },
    };
    let mut names: Vec<String> = path.iter().map(name).collect();
    if let Some(first) = names.first().cloned() {
        names.push(first);
    }
    names.join(" → ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::apply;
    use crate::compile::tests::{app, dev, world};
    use crate::state::{DeviceInfo, Observation};
    use crate::view::project;
    use sb_shared::ui::{Command, ErrorKind};

    fn route(from: Source, to: &str) -> Command {
        Command::SetRoute {
            from,
            to: Sink::Device(dev(to)),
            gain: Some(1.0),
        }
    }

    #[test]
    fn interface_1_cant_play_the_system_input_into_its_own_cable() {
        let mut s = world();
        apply(
            &mut s,
            Command::SetSystemInput(Some(SystemInput::Device(dev("cable-out")))),
        )
        .unwrap();
        let v = project(&s);
        assert!(v.devices[&dev("cable-in")].loops && !v.devices[&dev("speakers")].loops);
        let err = apply(
            &mut s,
            Command::SelectOutput {
                device: dev("cable-in"),
                selected: true,
            },
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Invalid);
        assert!(
            err.message
                .ends_with("the PC's sound (from cable-out) → cable-in → the PC's sound (from cable-out)"),
            "{}",
            err.message
        );
        assert!(s.desired.outputs.is_empty(), "undone");
        // The other way round: cable-in selected first, then the cable as the system input.
        apply(&mut s, Command::SetSystemInput(None)).unwrap();
        apply(
            &mut s,
            Command::SelectOutput {
                device: dev("cable-in"),
                selected: true,
            },
        )
        .unwrap();
        assert!(
            apply(
                &mut s,
                Command::SetSystemInput(Some(SystemInput::Device(dev("cable-out"))))
            )
            .is_err()
        );
        assert_eq!(s.desired.system_input, None);
    }

    #[test]
    fn the_matrix_refuses_loops_and_marks_the_cells_that_would_close_one() {
        let mut s = world();
        // A cable's output straight back into its input.
        assert!(apply(&mut s, route(Source::Device(dev("cable-out")), "cable-in")).is_err());
        // Mic → cable is fine; cable → speakers is fine.
        apply(&mut s, route(Source::Device(dev("mic")), "cable-in")).unwrap();
        apply(&mut s, route(Source::Device(dev("cable-out")), "speakers")).unwrap();
        let v = project(&s);
        assert!(
            v.would_loop
                .contains(&(Source::Device(dev("cable-out")), Sink::Device(dev("cable-in"))))
        );
        assert!(
            !v.would_loop
                .contains(&(Source::Device(dev("mic")), Sink::Device(dev("speakers"))))
        );
        assert_eq!(find(&s), None);
    }

    #[test]
    fn an_app_cant_be_sent_into_the_endpoint_it_plays_from() {
        let mut s = world();
        // The driver's endpoint 1 is a Windows render device too; Discord plays into it.
        let ep = dev("sb-endpoint-1");
        s.observe(Observation::DeviceAdded {
            key: ep.clone(),
            info: DeviceInfo {
                name: "Soundboard 1".into(),
                flow: Flow::Render,
            },
        });
        s.observe(Observation::Loopback {
            render: ep,
            into: Some(Loopback::Endpoint(1)),
        });
        s.desired.app_endpoints.insert(app("discord"), 1);
        let err = apply(&mut s, route(Source::App(app("discord")), "sb-endpoint-1")).unwrap_err();
        assert!(err.message.contains("discord"), "{}", err.message);
        // Through two hops: game → endpoint 1 (Discord's) → ... → game needs Discord → game's
        // own device, which doesn't exist; so this one is fine.
        apply(&mut s, route(Source::App(app("game")), "sb-endpoint-1")).unwrap();
        assert!(
            project(&s)
                .would_loop
                .contains(&(Source::App(app("discord")), Sink::Device(dev("sb-endpoint-1"))))
        );
    }

    #[test]
    fn a_longer_loop_through_two_cables_is_found() {
        let mut s = world();
        for (name, flow) in [("cable2-in", Flow::Render), ("cable2-out", Flow::Capture)] {
            s.observe(Observation::DeviceAdded {
                key: dev(name),
                info: DeviceInfo {
                    name: name.into(),
                    flow,
                },
            });
        }
        s.observe(Observation::Loopback {
            render: dev("cable2-in"),
            into: Some(Loopback::Capture(dev("cable2-out"))),
        });
        apply(&mut s, route(Source::Device(dev("cable-out")), "cable2-in")).unwrap();
        let err = apply(&mut s, route(Source::Device(dev("cable2-out")), "cable-in")).unwrap_err();
        assert_eq!(err.message.matches('→').count(), 4, "{}", err.message);
    }
}

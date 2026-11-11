//! Where the states come from: the service's status pipe.

use crossc_status::pipe::{Client, ClientEvent, ClientOptions, Problem, Trust};

use crate::view::{Feed, Outage};

/// What a problem of the pipe client means for the display. `None`: it is
/// not something to show (the client was stopped).
pub fn outage(problem: &Problem) -> Option<Outage> {
    Some(match problem {
        Problem::NotRunning => Outage::NotRunning,
        Problem::Busy => Outage::Busy,
        Problem::Denied => Outage::Denied,
        Problem::Untrusted => Outage::Untrusted,
        Problem::Version { theirs } => Outage::Version { theirs: *theirs },
        Problem::Malformed(_) => Outage::Unreadable,
        Problem::Silent | Problem::Lost(_) => Outage::Lost,
        Problem::Stopped => return None,
    })
}

/// The state a client event stands for.
pub fn feed(event: ClientEvent) -> Option<Feed> {
    match event {
        ClientEvent::Snapshot(s) => Some(Feed::Live(s)),
        ClientEvent::Problem(p) => outage(&p).map(Feed::Down),
    }
}

/// Whose pipe to believe. The service's own pipe must be the service's
/// (owned by SYSTEM or Administrators): any program can create a pipe of
/// that name while the service is not running. A pipe named on the
/// command line is somebody's test, and may be the user's own.
pub fn trust(pipe: Option<&str>) -> Trust {
    match pipe {
        None => Trust::Service,
        Some(_) => Trust::ServiceOrSelf,
    }
}

/// Keep a connection to the service (on `pipe`, or the service's own
/// pipe) and hand every state to `sink`, from the client's thread. The
/// connection lives as long as the returned client.
pub fn connect(
    pipe: Option<String>,
    mut sink: impl FnMut(Feed) + Send + 'static,
) -> Option<Client> {
    let mut options = ClientOptions {
        trust: trust(pipe.as_deref()),
        ..ClientOptions::default()
    };
    if let Some(p) = pipe {
        options.pipe_name = p;
    }
    Client::spawn(options, move |event| {
        if let Some(f) = feed(event) {
            sink(f);
        }
    })
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{Glyph, glyph};
    use crossc_status::Snapshot;

    #[test]
    fn every_problem_has_its_display() {
        let cases = [
            (Problem::NotRunning, Some(Outage::NotRunning), Glyph::Off),
            (Problem::Busy, Some(Outage::Busy), Glyph::Waiting),
            (Problem::Denied, Some(Outage::Denied), Glyph::Problem),
            (Problem::Untrusted, Some(Outage::Untrusted), Glyph::Problem),
            (
                Problem::Version { theirs: 4 },
                Some(Outage::Version { theirs: 4 }),
                Glyph::Problem,
            ),
            (
                Problem::Malformed("x".into()),
                Some(Outage::Unreadable),
                Glyph::Problem,
            ),
            (Problem::Silent, Some(Outage::Lost), Glyph::Waiting),
            (
                Problem::Lost("x".into()),
                Some(Outage::Lost),
                Glyph::Waiting,
            ),
        ];
        for (problem, want, icon) in cases {
            assert_eq!(outage(&problem), want, "{problem}");
            let f = feed(ClientEvent::Problem(problem)).unwrap();
            assert_eq!(glyph(&f), icon);
        }
        assert_eq!(outage(&Problem::Stopped), None);
        assert_eq!(feed(ClientEvent::Problem(Problem::Stopped)), None);
        let s = Box::new(Snapshot::default());
        assert_eq!(feed(ClientEvent::Snapshot(s.clone())), Some(Feed::Live(s)));
    }

    #[test]
    fn only_a_pipe_named_by_hand_may_be_the_users_own() {
        assert_eq!(trust(None), Trust::Service);
        assert_eq!(
            trust(Some(r"\\.\pipe\crossc-status-try")),
            Trust::ServiceOrSelf
        );
    }
}

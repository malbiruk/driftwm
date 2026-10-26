//! Closing a window while it is fit. A snapped fit pushes the window's cluster
//! aside, and the unfit that brings it home never runs for a window that goes
//! away, so the close pulls the cluster back the way the unfit would. A suspend
//! leaves it, since the stand-in keeps the fitted spot, and a close that pushed
//! nothing moves nothing: closes don't tidy gaps.

use driftwm::config::Action;
use driftwm::desktop_entry::DesktopEntryCache;
use smithay::desktop::Window;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use wayland_client::protocol::wl_surface::WlSurface;

use crate::state::StageWindow;

use super::client::ClientId;
use super::real::TempDir;
use super::{
    Fixture, adopt_last_configure, map_window, settle, tick_until_settled, window_by_app_id,
};

/// Map `app_id` at `size` and park it at canvas `pos`.
fn map_at(
    f: &mut Fixture,
    id: ClientId,
    app_id: &str,
    size: (u16, u16),
    pos: (i32, i32),
) -> (WlSurface, Window) {
    let surface = map_window(f, id, app_id, size);
    let window = window_by_app_id(f, app_id).unwrap();
    f.state()
        .map_window(StageWindow::Client(window.clone()), Point::from(pos), false);
    (surface, window)
}

/// Client-initiated close, driven to the server.
fn client_close(f: &mut Fixture, id: ClientId, surface: &WlSurface) {
    f.client(id).window(surface).destroy();
    f.roundtrip(id);
    f.dispatch();
}

fn position(f: &mut Fixture, window: &Window) -> Point<i32, Logical> {
    f.state().stage.position_of(window).unwrap()
}

/// `primary` (400x300) with `neighbour` (300x300) docked a gap to its right.
/// Returns the primary's surface, both windows, and where the neighbour docked.
fn docked_pair(f: &mut Fixture, id: ClientId) -> (WlSurface, Window, Window, Point<i32, Logical>) {
    let gap = f.state().config.snap_gap as i32;
    let (surface, primary) = map_at(f, id, "primary", (400, 300), (900, 300));
    let (_, neighbour) = map_at(f, id, "neighbour", (300, 300), (900 + 400 + gap, 300));
    settle(f);
    tick_until_settled(f);
    let docked = position(f, &neighbour);
    (surface, primary, neighbour, docked)
}

/// Run a fit to completion: the configure acked and the camera landed.
fn fit_and_settle(f: &mut Fixture, id: ClientId, surface: &WlSurface, snapped: bool, w: &Window) {
    if snapped {
        f.state().fit_window_snapped(w);
    } else {
        f.state().fit_window(w);
    }
    f.double_roundtrip(id);
    adopt_last_configure(f, id, surface);
    settle(f);
    tick_until_settled(f);
}

#[test]
fn closing_a_snapped_fit_window_pulls_its_neighbour_back() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();
    let (surface, primary, neighbour, docked) = docked_pair(&mut f, id);
    fit_and_settle(&mut f, id, &surface, true, &primary);
    assert_ne!(
        position(&mut f, &neighbour),
        docked,
        "precondition: the fit pushed the neighbour aside"
    );
    // Zoomed out so the pushed neighbour is on screen: an element that starts
    // off screen moves without a slide.
    f.state().with_output_state(|os| {
        os.zoom = 0.5;
        os.camera = Point::from((0.0, -500.0));
    });

    client_close(&mut f, id, &surface);

    assert_eq!(
        position(&mut f, &neighbour),
        docked,
        "the close pulled the neighbour back to where it docked"
    );
    let eid = f
        .state()
        .stage
        .id_of(&StageWindow::Client(neighbour))
        .unwrap();
    assert!(
        f.state().window_animations.generation_of(eid).is_some(),
        "and it slides back rather than jumping"
    );
}

/// Under the defaults, an app that opens maximized is placed beside the
/// focused window and its queued maximize fits the cluster, pushing that window
/// aside. Closing the app puts it back.
#[test]
fn closing_an_app_that_opened_maximized_puts_its_neighbour_back() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();
    map_window(&mut f, id, "terminal", (600, 400));
    let terminal = window_by_app_id(&mut f, "terminal").unwrap();
    settle(&mut f);
    let serial = SERIAL_COUNTER.next_serial();
    f.state().raise_and_focus(&terminal, serial);
    let before = position(&mut f, &terminal);

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id("browser");
    window.set_maximized();
    window.commit();
    f.roundtrip(id);
    let window = f.client(id).window(&surface);
    window.set_size(800, 600);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    settle(&mut f);
    assert_ne!(
        position(&mut f, &terminal),
        before,
        "precondition: the browser's maximize pushed the terminal aside"
    );

    client_close(&mut f, id, &surface);

    assert_eq!(position(&mut f, &terminal), before);
}

#[test]
fn closing_an_initially_maximized_window_leaves_later_neighbours_in_place() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.set_app_id("browser");
    window.set_maximized();
    window.commit();
    f.roundtrip(id);
    let window = f.client(id).window(&surface);
    window.set_size(800, 600);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    settle(&mut f);

    let browser = window_by_app_id(&mut f, "browser").unwrap();
    map_window(&mut f, id, "terminal", (600, 400));
    let terminal = window_by_app_id(&mut f, "terminal").unwrap();
    settle(&mut f);
    let before = position(&mut f, &terminal);
    let browser_rect = f
        .state()
        .visual_frame_rect(&StageWindow::Client(browser))
        .unwrap();
    let terminal_rect = f
        .state()
        .visual_frame_rect(&StageWindow::Client(terminal.clone()))
        .unwrap();
    assert!(
        driftwm::layout::cluster::adjacent_side(
            &browser_rect,
            &terminal_rect,
            f.state().config.snap_gap
        )
        .is_some(),
        "the terminal docked against the maximized browser"
    );

    client_close(&mut f, id, &surface);

    assert_eq!(
        position(&mut f, &terminal),
        before,
        "the browser never pushed this terminal, so closing must not pull it back"
    );
}

#[test]
fn suspending_a_snapped_fit_window_leaves_its_neighbour_where_the_fit_put_it() {
    let tmp = TempDir::new();
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    std::fs::write(
        tmp.path().join("primary.desktop"),
        "[Desktop Entry]\nType=Application\nName=primary\nExec=primary\n",
    )
    .unwrap();
    f.state().desktop_entry_cache = Some(DesktopEntryCache::new(vec![tmp.path().to_path_buf()]));
    let id = f.add_client();
    let (surface, primary, neighbour, _) = docked_pair(&mut f, id);
    fit_and_settle(&mut f, id, &surface, true, &primary);
    let pushed = position(&mut f, &neighbour);

    f.state().execute_action(&Action::SuspendWindow);
    client_close(&mut f, id, &surface);

    assert!(
        f.state().stage.windows().any(|w| w.suspended().is_some()),
        "precondition: a stand-in took the window's place"
    );
    assert_eq!(
        position(&mut f, &neighbour),
        pushed,
        "the stand-in keeps the fitted spot, so the neighbour stays clear of it"
    );
}

#[test]
fn closing_an_unfit_window_leaves_the_gap() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();
    let (surface, _, neighbour, docked) = docked_pair(&mut f, id);

    client_close(&mut f, id, &surface);

    assert_eq!(position(&mut f, &neighbour), docked);
}

/// A plain fit pushes nothing, so its close pulls nothing — not even a window
/// docked against the fitted window afterwards.
#[test]
fn closing_a_plain_fit_window_leaves_a_window_docked_to_it() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let gap = f.state().config.snap_gap as i32;
    let id = f.add_client();
    let (surface, primary) = map_at(&mut f, id, "primary", (400, 300), (900, 300));
    fit_and_settle(&mut f, id, &surface, false, &primary);
    let fit_loc = position(&mut f, &primary);
    let fit_size = crate::state::configured_window_size(&primary);
    let (_, neighbour) = map_at(
        &mut f,
        id,
        "neighbour",
        (300, 300),
        (fit_loc.x + fit_size.w + gap, fit_loc.y),
    );
    tick_until_settled(&mut f);
    let docked = position(&mut f, &neighbour);

    client_close(&mut f, id, &surface);

    assert_eq!(position(&mut f, &neighbour), docked);
}

/// A fit window that went fullscreen is still fit underneath, and its close
/// still owes the cluster the fit pushed.
#[test]
fn closing_a_fit_window_from_fullscreen_pulls_its_neighbour_back() {
    let mut f = Fixture::new();
    let output = f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let id = f.add_client();
    let (surface, primary, neighbour, docked) = docked_pair(&mut f, id);
    fit_and_settle(&mut f, id, &surface, true, &primary);
    f.state().enter_fullscreen(&primary, Some(output));
    f.double_roundtrip(id);
    adopt_last_configure(&mut f, id, &surface);
    assert_ne!(
        position(&mut f, &neighbour),
        docked,
        "precondition: the fit pushed the neighbour aside"
    );

    client_close(&mut f, id, &surface);

    assert_eq!(position(&mut f, &neighbour), docked);
}

/// A client that disconnects instead of closing has its title bar torn down
/// before the close runs; the neighbour docked against that bar comes back
/// with the rest.
#[test]
fn a_disconnect_pulls_back_the_neighbour_docked_above_the_title_bar() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    f.skip_baseline_check();
    let gap = f.state().config.snap_gap as i32;
    let bar = f.state().config.decorations.title_bar_height;
    let app = f.add_client();
    let (surface, primary) = map_at(&mut f, app, "primary", (400, 300), (900, 300));
    super::give_ssd(&mut f, &primary);
    let other = f.add_client();
    let (_, neighbour) = map_at(
        &mut f,
        other,
        "neighbour",
        (300, 300),
        (900, 300 - bar - gap - 300),
    );
    settle(&mut f);
    tick_until_settled(&mut f);
    let docked = position(&mut f, &neighbour);
    fit_and_settle(&mut f, app, &surface, true, &primary);
    assert_ne!(
        position(&mut f, &neighbour),
        docked,
        "precondition: the fit pushed the neighbour aside"
    );

    f.kill_client(app);
    f.pump(3);

    assert_eq!(position(&mut f, &neighbour), docked);
}

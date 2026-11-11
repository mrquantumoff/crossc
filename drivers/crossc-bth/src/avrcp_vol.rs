//! Unified volume: the state behind the endpoint's hardware volume and the
//! inbox AVRCP volume contract, as plain data and plain functions.
//!
//! No kernel calls, no `unsafe`, no floating point, no allocation, no
//! panics. The driver (`acx.rs`, `audio.rs`) keeps one [`Volume`] per device
//! under its audio spinlock and calls into it from the ACX callbacks; the
//! same file is compiled into the host test crate
//! (`drivers/crossc-bth/host-tests`, `volume_tests.rs`).
//!
//! # The two sides
//! * **Windows** (audiosrv, through the ACX volume and mute elements): the
//!   slider writes a level per channel ([`Volume::windows_level`]) and a
//!   mute state ([`Volume::windows_mute`]), and reads them back.
//! * **AVRCP** (`BthAvrcp.dll`'s `VolumeController` inside BthAvctpSvc,
//!   through the private KS property / event `{401e566b-...}` id 1 on the
//!   render circuit, see `docs/DESIGN.md` "Volume"): it enables the event
//!   ([`Volume::subscribe`]), reads the level when the event fires
//!   ([`Volume::remote_get`]) and sends it to the buds as SetAbsoluteVolume,
//!   and writes the buds' own level when they report one
//!   ([`Volume::remote_set`]: INTERIM at connect, CHANGED on a gesture).
//!
//! # Units and mapping
//! Levels are LONG 1/65536 dB in [`LEVEL_MIN`] (-63.5 dB, `0xFFC08000`) ..=
//! [`LEVEL_MAX`] (0 dB), step [`LEVEL_STEP`] (0.5 dB): the range, step and
//! clamp of the inbox `BthA2dp.sys` (`AVFilter::GetVolumePropertyDescription`
//! data, `A2dpRole::SetVolume`). BthAvrcp converts between that and AVRCP
//! 0..=127 with the audio taper `dB(v) = 35 * log10(lo + v/127 * (1 - lo))`,
//! `lo = 10^(-63.5/35)` (`AvrcpHelpers::AvrcpVolumeToDecibels`, exponent 1.75
//! over -63.5..0 dB), and back with `round(127 * wiper) & 0x7F`. audiosrv
//! maps the slider onto a hardware range with the same taper, so a slider
//! position p written exactly arrives at the buds as AVRCP `round(127 p)`.
//! If audiosrv writes on the volume element's 0.5 dB grid, about half the
//! positions land one or two AVRCP steps away from that (cosmetic, no
//! oscillation; the host tests bound it). The two tables
//! below are that taper in integers (the driver has no floating point); the
//! host tests check them against the closed form.
//!
//! Every level handed to AVRCP is clamped to the range: BthAvrcp masks its
//! conversion with `& 0x7F`, so a level above 0 dB would wrap to AVRCP 0 and
//! one far below -63.5 dB to 127.
//!
//! # Rules
//! * **Windows -> buds.** A Windows write changes the level (the louder of
//!   the two channels; AVRCP has one volume). If AVRCP is subscribed and the
//!   endpoint is not muted, the AVRCP event is generated; BthAvrcp then reads
//!   the level and sends SetAbsoluteVolume if the AVRCP value differs from
//!   what it last sent or heard.
//! * **Buds -> Windows.** A SET from AVRCP stores the buds' level for both
//!   channels and Windows is told the level changed (the slider moves). The
//!   AVRCP event is not generated for it.
//! * **Echo rule** (no oscillation between the slider's 0.5 dB grid and the
//!   buds' 1/127 steps). After a buds level R, Windows typically writes back
//!   R rounded to its own grid. A Windows level within one step (less than
//!   [`LEVEL_STEP`]) of R is taken as that write-back: the channels take it,
//!   the AVRCP level stays R exactly, no event. Near the top of the range
//!   one AVRCP step is 0.12 dB, so without this rule the rounded value would
//!   map to a different AVRCP value and the buds would be sent a step they
//!   did not ask for. The rule ends with the first Windows level one step
//!   or more away from R (a real slider move), and [`ECHO_MS`] after the SET
//!   in any case, so a small slider move later (a volume key near the top
//!   is less than 0.5 dB) is never swallowed. A write-back that comes later
//!   than that is sent to the buds as a real move (one spurious step). For
//!   buds that report only real changes that is the end of it. Buds that
//!   snap SetAbsoluteVolume to a coarser grid *and* report CHANGED even when
//!   nothing changed answer with a different value, which is taken as a
//!   gesture; if every write-back keeps arriving after [`ECHO_MS`], that
//!   repeats with each one. This rule does not settle that case; audiosrv
//!   is expected to write back well within [`ECHO_MS`].
//! * **Confirmations.** A SET whose AVRCP value equals the current level's
//!   (the buds confirming a level Windows set) changes nothing, so the
//!   slider does not snap to the buds' coarser value.
//! * **Connect.** The first SET after the event is enabled is BthAvrcp's
//!   cached AVRCP byte, written by `EnableResponseProcessingEntry`; it
//!   always wins, as with the inbox driver. Whether that byte is already
//!   the buds' level depends on an order this driver cannot see: if the
//!   INTERIM came first it is (one SET); if response processing was enabled
//!   first it is the `VolumeController` constructor's default
//!   ([`BTHAVRCP_DEFAULT_AVRCP`], 64), and the INTERIM follows as a second
//!   SET (`OnInterimReceivedInternal` SETs only once processing is on, and
//!   only when the integer percent differs from the cached byte's). So a
//!   first SET of exactly AVRCP 64 opens a connect window: the next SET
//!   within [`CONNECT_MS`] is taken as the connect level too (it wins; a
//!   mute stays and is re-sent), except AVRCP 0 while muted after AVRCP
//!   has read the muted minimum (the buds following the mute) and a repeat
//!   of the current value, which are ignored and keep the window open. A Windows level change that is sent
//!   to AVRCP (a real slider move) closes the window early; a Windows mute
//!   change does not, so muting during the window cannot make the INTERIM
//!   look like a gesture that unmutes.
//! * **Mute** (mute element; same as BthA2dp, which reports the minimum to
//!   AVRCP while muted). Muting or unmuting generates the event; while muted
//!   AVRCP reads [`LEVEL_MIN`] (AVRCP 0) and Windows writes only move the
//!   stored level. A SET at AVRCP 0 while muted is the buds following the
//!   mute and is ignored. Any other SET while muted is a volume gesture on
//!   the buds and unmutes, except a connect SET (see **Connect**), which
//!   only takes the level and re-sends the mute (event).
//! * **Active** ([`Volume::active`]): the volume element exists, the event is
//!   enabled, and the subscriber answers. ACX calls the enable callback for
//!   the first subscriber and the disable callback when the last one goes,
//!   also when its handle is closed (`Acx01000!AfxEvent::EnableEvent` /
//!   `DisableEvent` count subscribers; `AfxCircuit::FileClose` disables a
//!   closed file's events). The answer check: an event that has had no GET
//!   or SET for [`STALE_MS`] when the next one is due marks the subscriber
//!   stale (not active) until it reads or writes again. Not active means the
//!   service applies the level in software, so a silent subscriber costs a
//!   dead slider for at most one change.

use crossc_ioctl::audio::{
    AUDIO_FLAG_AVRCP, AUDIO_FLAG_AVRCP_SUBSCRIBED, AUDIO_FLAG_HW_VOLUME, AUDIO_FLAG_MUTED, VOLUME_LEVEL_MAX,
    VOLUME_LEVEL_MIN, VolumeState,
};

/// -63.5 dB in 1/65536 dB (`0xFFC08000`).
pub const LEVEL_MIN: i32 = VOLUME_LEVEL_MIN;
/// 0 dB.
pub const LEVEL_MAX: i32 = VOLUME_LEVEL_MAX;
/// 0.5 dB: the stepping the volume element declares.
pub const LEVEL_STEP: i32 = 0x8000;
/// Channels of the volume and mute elements (stereo, like BthA2dp).
pub const CHANNELS: usize = 2;
/// The channel index ACX passes for "all channels" (master).
pub const CHANNEL_MASTER: u32 = u32::MAX;
/// An AVRCP event unanswered (no GET, no SET) this long when the next one is
/// due marks the subscriber stale.
pub const STALE_MS: u64 = 2000;
/// How long after a buds level Windows' write-back of it is expected.
pub const ECHO_MS: u64 = 1000;
/// The AVRCP byte a new BthAvrcp `VolumeController` caches before it has
/// heard the buds (`mov word ptr [rdi+0E8h],4000h` in the constructor:
/// response processing off, cached volume 0x40).
pub const BTHAVRCP_DEFAULT_AVRCP: u8 = 64;
/// How long after a connect SET of [`BTHAVRCP_DEFAULT_AVRCP`] the buds'
/// INTERIM level may still follow as the connect level.
pub const CONNECT_MS: u64 = 3000;
/// Level before Windows or the buds set one: AVRCP 64 (-10.2 dB, slider
/// 50 %). Only matters if AVRCP reads before audiosrv restores the
/// endpoint's volume; 0 dB there would send the buds to full volume.
pub const INITIAL_LEVEL: i32 = LEVEL_OF_AVRCP[64];

/// AVRCP value v -> level: `trunc(65536 * 35 * log10(lo + v/127 * (1 - lo)))`,
/// the ends pinned to the range. What BthAvrcp writes for a buds level v.
pub const LEVEL_OF_AVRCP: [i32; 128] = [
    -4161536, -3753941, -3465525, -3242136, -3059780, -2905697, -2772285, -2654649, //
    -2549448, -2454304, -2367460, -2287583, -2213639, -2144806, -2080423, -2019950, //
    -1962939, -1909015, -1857861, -1809206, -1762816, -1718492, -1676056, -1635353, //
    -1596249, -1558622, -1522365, -1487381, -1453585, -1420897, -1389248, -1358573, //
    -1328815, -1299920, -1271840, -1244530, -1217948, -1192057, -1166822, -1142211, //
    -1118193, -1094741, -1071828, -1049430, -1027525, -1006091, -985108, -964559, //
    -944425, -924689, -905337, -886354, -867726, -849440, -831483, -813845, //
    -796513, -779478, -762729, -746257, -730053, -714108, -698415, -682965, //
    -667751, -652766, -638002, -623455, -609117, -594982, -581045, -567301, //
    -553743, -540367, -527169, -514143, -501286, -488592, -476058, -463680, //
    -451453, -439375, -427442, -415650, -403995, -392476, -381088, -369829, //
    -358696, -347686, -336796, -326024, -315367, -304823, -294390, -284064, //
    -273845, -263729, -253715, -243801, -233984, -224263, -214636, -205101, //
    -195657, -186301, -177033, -167850, -158750, -149733, -140797, -131941, //
    -123162, -114460, -105834, -97281, -88802, -80393, -72056, -63787, //
    -55587, -47453, -39386, -31383, -23444, -15568, -7753, 0, //
];

/// `AVRCP_BOUNDARY[v]`: the lowest level that converts to AVRCP v + 1, i.e.
/// `ceil(65536 * 35 * log10(lo + (v + 0.5)/127 * (1 - lo)))` (BthAvrcp
/// rounds half away from zero).
pub const AVRCP_BOUNDARY: [i32; 127] = [
    -3937035, -3599331, -3347582, -3146791, -2979763, -2836760, -2711732, -2600661, //
    -2500741, -2409936, -2326721, -2249925, -2178628, -2112095, -2049728, -1991037, //
    -1935613, -1883110, -1833236, -1785741, -1740408, -1697048, -1655497, -1615610, //
    -1577258, -1540329, -1504720, -1470340, -1437107, -1404947, -1373792, -1343583, //
    -1314263, -1285781, -1258091, -1231150, -1204919, -1179360, -1154441, -1130130, //
    -1106398, -1083218, -1060566, -1038417, -1016750, -995544, -974781, -954441, //
    -934508, -914966, -895801, -876997, -858541, -840421, -822625, -805141, //
    -787959, -771068, -754459, -738122, -722049, -706231, -690660, -675329, //
    -660230, -645357, -630702, -616260, -602024, -587989, -574149, -560499, //
    -547033, -533746, -520635, -507694, -494919, -482305, -469850, -457548, //
    -445396, -433391, -421528, -409806, -398219, -386766, -375443, -364247, //
    -353176, -342226, -331395, -320681, -310081, -299593, -289214, -278941, //
    -268774, -258709, -248745, -238880, -229112, -219438, -209857, -200368, //
    -190968, -181656, -172431, -163289, -154232, -145255, -136359, -127542, //
    -118802, -110138, -101548, -93032, -84589, -76216, -67913, -59679, //
    -51512, -43411, -35376, -27405, -19498, -11653, -3869, //
];

/// Any LONG -> a level in range.
pub fn clamp_level(level: i32) -> i32 {
    level.clamp(LEVEL_MIN, LEVEL_MAX)
}

/// AVRCP absolute volume (bit 7 ignored, as BthAvrcp does) -> level.
#[cfg_attr(not(test), allow(dead_code))] // the host tests use it
pub fn avrcp_to_level(v: u8) -> i32 {
    LEVEL_OF_AVRCP.get(usize::from(v & 0x7F)).copied().unwrap_or(LEVEL_MIN)
}

/// Level (clamped first) -> the AVRCP value BthAvrcp sends for it.
pub fn level_to_avrcp(level: i32) -> u8 {
    let l = clamp_level(level);
    // At most 127 boundaries are <= l.
    AVRCP_BOUNDARY.partition_point(|&b| b <= l) as u8
}

/// What the driver has to do after a call, besides storing the state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    /// Tell Windows the volume level changed (`AcxVolumeChangeLevelNotification`).
    pub notify_volume: bool,
    /// Tell Windows the mute state changed (`AcxMuteChangeStateNotification`).
    pub notify_mute: bool,
    /// Generate the AVRCP event (`AcxEventGenerateEvent`, `{401e566b}` id 1).
    pub avrcp_event: bool,
}

/// A channel index the elements do not have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadChannel;

/// The volume state of one device. Lives as long as the device: a circuit
/// that is rebuilt keeps the level and the mute state.
#[derive(Clone, Copy, Debug)]
pub struct Volume {
    /// The volume element exists (circuit added with the feature on).
    element: bool,
    /// Levels as Windows set them, per channel.
    channels: [i32; CHANNELS],
    /// The level for AVRCP (unmuted value).
    level: i32,
    mute: [bool; CHANNELS],
    /// The AVRCP event is enabled.
    subscribed: bool,
    /// The next SET is the connect-time level.
    sync_pending: bool,
    /// Connect window (ms, caller's clock): until then a SET may still be
    /// the buds' INTERIM level following BthAvrcp's default.
    connect_until: Option<u64>,
    /// AVRCP has read the muted minimum since the connect SET.
    min_read: bool,
    /// Buds level whose Windows write-back is expected (echo rule), and
    /// when it was set (ms).
    echo_ref: Option<(i32, u64)>,
    /// Since when an event waits for its GET (ms, caller's clock).
    pending_since: Option<u64>,
    /// The subscriber left an event unanswered for [`STALE_MS`].
    stale: bool,
    gets: u32,
    sets: u32,
    events: u32,
}

impl Default for Volume {
    fn default() -> Self {
        Self::new()
    }
}

impl Volume {
    pub const fn new() -> Self {
        Self {
            element: false,
            channels: [INITIAL_LEVEL; CHANNELS],
            level: INITIAL_LEVEL,
            mute: [false; CHANNELS],
            subscribed: false,
            sync_pending: false,
            connect_until: None,
            min_read: false,
            echo_ref: None,
            pending_since: None,
            stale: false,
            gets: 0,
            sets: 0,
            events: 0,
        }
    }

    /// Forget everything that belongs to a circuit's subscribers.
    fn drop_subscriber(&mut self) {
        self.subscribed = false;
        self.sync_pending = false;
        self.connect_until = None;
        self.echo_ref = None;
        self.pending_since = None;
        self.stale = false;
    }

    /// The volume element came (circuit added) or went (circuit removed:
    /// its events and their subscribers are gone with it).
    pub fn set_element(&mut self, present: bool) {
        self.element = present;
        if !present {
            self.drop_subscriber();
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // the host tests use it
    pub fn element(&self) -> bool {
        self.element
    }

    pub fn muted(&self) -> bool {
        self.mute.iter().any(|&m| m)
    }

    #[cfg_attr(not(test), allow(dead_code))] // the host tests use it
    pub fn level(&self) -> i32 {
        self.level
    }

    /// A connect SET of BthAvrcp's default has been seen and the buds'
    /// INTERIM may still follow (see **Connect**; for the trace).
    pub fn connect_window_open(&self) -> bool {
        self.connect_until.is_some()
    }

    #[cfg_attr(not(test), allow(dead_code))] // the host tests use it
    pub fn subscribed(&self) -> bool {
        self.subscribed
    }

    /// Hardware volume is in effect: the buds apply the level.
    pub fn active(&self) -> bool {
        self.element && self.subscribed && !self.stale
    }

    /// The level AVRCP reads: the minimum while muted.
    fn avrcp_level(&self) -> i32 {
        if self.muted() { LEVEL_MIN } else { clamp_level(self.level) }
    }

    /// An AVRCP event is due. Returns whether to generate it (only while
    /// subscribed); notes how long the previous one has waited.
    fn want_event(&mut self, now_ms: u64) -> bool {
        if !self.element || !self.subscribed {
            return false;
        }
        match self.pending_since {
            Some(t) if now_ms.saturating_sub(t) >= STALE_MS => self.stale = true,
            Some(_) => {}
            None => self.pending_since = Some(now_ms),
        }
        true
    }

    /// The subscriber answered (GET or SET).
    fn answered(&mut self) {
        self.pending_since = None;
        self.stale = false;
    }

    // ---- Windows side -------------------------------------------------------

    /// `EvtAcxVolumeAssignLevel`: Windows sets `channel` (or every channel,
    /// [`CHANNEL_MASTER`]) to `level`. `now_ms` is any monotonic clock.
    pub fn windows_level(&mut self, channel: u32, level: i32, now_ms: u64) -> Result<Effects, BadChannel> {
        let l = clamp_level(level);
        match channel {
            CHANNEL_MASTER => self.channels = [l; CHANNELS],
            c => *self.channels.get_mut(c as usize).ok_or(BadChannel)? = l,
        }
        let louder = self.channels[0].max(self.channels[1]);
        let mut fx = Effects::default();
        if let Some((r, t)) = self.echo_ref {
            if now_ms.saturating_sub(t) < ECHO_MS && (louder - r).abs() < LEVEL_STEP {
                // Windows writing the buds' level back on its own grid.
                return Ok(fx);
            }
            self.echo_ref = None;
        }
        if louder != self.level {
            self.level = louder;
            fx.avrcp_event = !self.muted() && self.want_event(now_ms);
            if fx.avrcp_event {
                // A real slider move: the connect phase is over.
                self.connect_until = None;
            }
        }
        Ok(fx)
    }

    /// `EvtAcxVolumeRetrieveLevel`.
    pub fn retrieve_level(&self, channel: u32) -> Result<i32, BadChannel> {
        match channel {
            CHANNEL_MASTER => Ok(self.channels[0].max(self.channels[1])),
            c => self.channels.get(c as usize).copied().ok_or(BadChannel),
        }
    }

    /// `EvtAcxMuteAssignState`.
    pub fn windows_mute(&mut self, channel: u32, on: bool, now_ms: u64) -> Result<Effects, BadChannel> {
        let was = self.muted();
        match channel {
            CHANNEL_MASTER => self.mute = [on; CHANNELS],
            c => *self.mute.get_mut(c as usize).ok_or(BadChannel)? = on,
        }
        let mut fx = Effects::default();
        if self.muted() != was {
            fx.avrcp_event = self.want_event(now_ms);
        }
        Ok(fx)
    }

    /// `EvtAcxMuteRetrieveState`.
    pub fn retrieve_mute(&self, channel: u32) -> Result<bool, BadChannel> {
        match channel {
            CHANNEL_MASTER => Ok(self.muted()),
            c => self.mute.get(c as usize).copied().ok_or(BadChannel),
        }
    }

    // ---- AVRCP side ---------------------------------------------------------

    /// The AVRCP event was enabled (first subscriber) or disabled (last one
    /// gone, also by its handle closing).
    pub fn subscribe(&mut self, on: bool) {
        if on {
            self.subscribed = true;
            self.sync_pending = true;
            self.connect_until = None;
            self.echo_ref = None;
            self.answered();
        } else {
            self.drop_subscriber();
        }
    }

    /// The driver generated the AVRCP event.
    pub fn event_generated(&mut self) {
        self.events = self.events.wrapping_add(1);
    }

    /// Property GET from AVRCP: the level to send to the buds.
    pub fn remote_get(&mut self) -> i32 {
        self.gets = self.gets.wrapping_add(1);
        self.answered();
        if self.muted() {
            self.min_read = true;
        }
        self.avrcp_level()
    }

    /// Property SET from AVRCP: the buds report `level` (any LONG; clamped).
    /// `now_ms` is the clock of [`Self::windows_level`].
    pub fn remote_set(&mut self, level: i32, now_ms: u64) -> Effects {
        self.sets = self.sets.wrapping_add(1);
        self.answered();
        let l = clamp_level(level);
        let v = level_to_avrcp(l);
        let mut fx = Effects::default();
        if core::mem::take(&mut self.sync_pending) {
            // Connect: BthAvrcp's cached level; it wins.
            self.connect_until = (v == BTHAVRCP_DEFAULT_AVRCP).then_some(now_ms.saturating_add(CONNECT_MS));
            self.min_read = false;
            return self.connect_level(l, now_ms);
        }
        if let Some(until) = self.connect_until.take()
            && now_ms < until
        {
            // AVRCP 0 while muted is the buds following the mute only once
            // AVRCP has read the minimum; before that it is their level.
            if (self.muted() && v == 0 && self.min_read) || v == level_to_avrcp(self.level) {
                // The buds following the mute, or a repeat: still waiting.
                // A non-zero repeat while muted has just become BthAvrcp's
                // cached byte, so the mute is re-sent.
                self.connect_until = Some(until);
                fx.avrcp_event = self.muted() && v != 0;
                return fx;
            }
            // The buds' INTERIM after BthAvrcp's default: the real
            // connect level.
            return self.connect_level(l, now_ms);
        }
        if self.muted() {
            if v == 0 {
                return fx; // the buds following the mute
            }
            self.mute = [false; CHANNELS];
            self.take_remote(l, now_ms);
            fx.notify_volume = true;
            fx.notify_mute = true;
            return fx;
        }
        if v == level_to_avrcp(self.level) {
            return fx; // a confirmation of the current level
        }
        self.take_remote(l, now_ms);
        fx.notify_volume = true;
        fx
    }

    /// A connect level: it wins; a mute stays and is re-sent.
    fn connect_level(&mut self, l: i32, now_ms: u64) -> Effects {
        self.take_remote(l, now_ms);
        Effects { notify_volume: true, notify_mute: false, avrcp_event: self.muted() }
    }

    fn take_remote(&mut self, l: i32, now_ms: u64) {
        self.level = l;
        self.channels = [l; CHANNELS];
        self.echo_ref = Some((l, now_ms));
    }

    // ---- report -------------------------------------------------------------

    /// The volume part of the audio state: `AUDIO_FLAG_*` volume bits and
    /// [`VolumeState`]. All zero without the element.
    pub fn wire(&self) -> (u32, VolumeState) {
        if !self.element {
            return (0, VolumeState::default());
        }
        let mut flags = AUDIO_FLAG_HW_VOLUME;
        if self.active() {
            flags |= AUDIO_FLAG_AVRCP;
        }
        if self.muted() {
            flags |= AUDIO_FLAG_MUTED;
        }
        if self.subscribed {
            flags |= AUDIO_FLAG_AVRCP_SUBSCRIBED;
        }
        let state = VolumeState {
            level: clamp_level(self.level),
            left: self.channels[0],
            right: self.channels[1],
            avrcp: level_to_avrcp(self.avrcp_level()),
            gets: self.gets as u16,
            sets: self.sets as u16,
            events: self.events as u16,
        };
        (flags, state)
    }
}

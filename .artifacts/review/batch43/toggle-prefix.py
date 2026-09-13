import sys
mode=sys.argv[1]  # l0104-on | l0104-off | l0105-on | l0105-off
p='crates/inf-server/src/durable.rs'; s=open(p).read()
q='crates/inf-log/src/commit.rs'; t=open(q).read()
H=[
 ('''            self.note_issued();
            let ticket = self.commit.register_standalone_fsync(cx.now);''',
  '''            self.frame_held = false; // PREFIX-L0104
            let ticket = self.commit.register_standalone_fsync(cx.now);'''),
 ('''            // The hold episode ends at the issue (`note_issued`), not
            // here: a reservation that waits below keeps the episode.
            let deferred = match self.rotor.begin_frame_deferred(frame_len, cx.now.as_millis()) {''',
  '''            self.frame_held = false; // PREFIX-L0104
            let deferred = match self.rotor.begin_frame_deferred(frame_len, cx.now.as_millis()) {'''),
 ('''        assert!(
            self.fill_since.is_none() || self.frame_held,
            "open fill episode without a held frame"
        );''',
  '''        // PREFIX-L0104 fill assert off'''),
 ('''        assert!(
            self.group_since.is_none() || self.frame_held,
            "open group-hold episode without a held frame"
        );''',
  '''        // PREFIX-L0104 group assert off'''),
]
G=[('''        if self.syncs_in_flight() + usize::from(seal_ahead) < self.flush_bound {''',
    '''        if self.syncs_in_flight() < self.flush_bound { // PREFIX-L0105''')]
if mode.startswith('l0104'):
    for a,b in H:
        old,new=(a,b) if mode.endswith('on') else (b,a)
        assert s.count(old)==1,(mode,old[:50]); s=s.replace(old,new)
    open(p,'w').write(s)
else:
    for a,b in G:
        old,new=(a,b) if mode.endswith('on') else (b,a)
        assert t.count(old)==1,(mode,old[:50]); t=t.replace(old,new)
    open(q,'w').write(t)
print(mode,'ok')

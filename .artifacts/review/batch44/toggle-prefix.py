import sys
mode=sys.argv[1]  # fifo-on | fifo-off  (the pre-fix ledger: no window in the plan, no assert at registration)
q='crates/inf-log/src/commit.rs'; t=open(q).read()
G=[
 ('''        if write_through_ok
            && (seal_ahead || self.write_through_due())
            && !self.write_through_window_full()
        {''',
  '''        if write_through_ok && (seal_ahead || self.write_through_due()) { // PREFIX-FIFO'''),
 ('''            assert!(
                self.write_through_pending < WRITE_THROUGH_WINDOW_ENTRIES,
                "write-through ticket into a full window"
            );
            self.write_through_pending += 1;''',
  '''            self.write_through_pending += 1; // PREFIX-FIFO assert off'''),
]
for a,b in G:
    old,new=(a,b) if mode.endswith('on') else (b,a)
    assert t.count(old)==1,(mode,old[:50]); t=t.replace(old,new)
open(q,'w').write(t)
print(mode,'ok')

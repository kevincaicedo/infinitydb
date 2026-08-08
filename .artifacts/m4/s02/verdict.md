IDENTICAL  <core::ptr::drop_in_place<inf_store::index::Index>>:
INLINED    <<inf_store::index::Index as core::fmt::Debug>::fmt>: (no standalone M4 copy; call sites compared above)
IDENTICAL  <inf_store::index::Index::insert>:
INLINED    <inf_store::index::Index::live_from>: (no standalone M4 copy; call sites compared above)
IDENTICAL  <inf_store::index::Index::position_of>:
IDENTICAL  <inf_store::index::Index::remove>:
INLINED    <inf_store::index::Index::replace>: (no standalone M4 copy; call sites compared above)
DIVERGED   <inf_store::index::Index::with_capacity>: (informational — constructor/diagnostic, off the hot path)
IDENTICAL  <inf_store::store::CellStore::get>:
IDENTICAL  <inf_store::store::CellStore::getdel>:
IDENTICAL  <inf_store::store::CellStore::get_ex>:
IDENTICAL  <inf_store::store::CellStore::get_range>:
IDENTICAL  <inf_store::store::CellStore::get_str>:
IDENTICAL  <inf_store::store::CellStore::get_with_hash>:
IDENTICAL  <inf_store::store::CellStore::resolve>:
IDENTICAL  <inf_store::store::CellStore::resolve_hashed>:
IDENTICAL  <inf_store::store::CellStore::set>:
IDENTICAL  <inf_store::store::CellStore::set_eviction_policy>:
IDENTICAL  <inf_store::store::CellStore::set_range>:
IDENTICAL  <inf_store::store::CellStore::write_record_carrying>:

M4-only blocks (new instantiations — runtime-dead in memory mode, proven by S03):
  <core::ptr::drop_in_place<inf_store::index::Index<inf_store::index::TieredMode>>>:
  <inf_store::index::Index::insert>:
  <inf_store::index::Index::position_of>:
  <inf_store::index::Index::with_capacity>:

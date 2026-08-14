<core::ptr::drop_in_place<inf_store::index::Index>>:
push   %rbx
mov    %rdi,%rbx
cmpq   $0x0,0x8(%rdi)
je     <core::ptr::drop_in_place<inf_store::index::Index>+OFF>
mov    (%rbx),%rdi
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
cmpq   $0x0,0x18(%rbx)
je     <core::ptr::drop_in_place<inf_store::index::Index>+OFF>
mov    0x10(%rbx),%rdi
pop    %rbx
jmp    *0x0(%rip)        # <free@GLIBC_2.2.5>
pop    %rbx
ret
int3
int3
int3
int3
int3
int3
int3
int3

<inf_store::index::Index<M>::position_of>:
push   %rbp
push   %r15
push   %r14
push   %r13
push   %r12
push   %rbx
push   %rax
mov    %rdx,%rcx
mov    %rdi,%rax
mov    0x8(%rdi),%rdx
mov    0x20(%rdi),%r9
shr    $0x4,%r9
dec    %r9
mov    %r9,%r10
and    %rsi,%r10
mov    %r10,%rdi
shl    $0x4,%rdi
lea    0xf(%rdi),%r8
cmp    %rdx,%r8
jae    <inf_store::index::Index<M>::position_of+OFF>
shr    $0x39,%rsi
mov    (%rax),%r11
mov    0x10(%rax),%rbx
movd   %esi,%xmm0
punpcklbw %xmm0,%xmm0
pshuflw $0x0,%xmm0,%xmm0
pshufd $0x44,%xmm0,%xmm0
mov    0x18(%rax),%rsi
xor    %eax,%eax
movdqa 0x0(%rip),%xmm1        # <anon.58baeeae7d5fe476157d296c9f08f803.346.llvm.5844774155465566772+OFF>
movabs $0xffffffffffff,%r14
xor    %r15d,%r15d
movdqu (%r11,%rdi,1),%xmm2
movdqa %xmm0,%xmm3
pcmpeqb %xmm2,%xmm3
pmovmskb %xmm3,%r12d
data16 cs nopw 0x0(%rax,%rax,1)
test   %r12w,%r12w
je     <inf_store::index::Index<M>::position_of+OFF>
tzcnt  %r12d,%r8d
or     %rdi,%r8
cmp    %rsi,%r8
jae    <inf_store::index::Index<M>::position_of+OFF>
lea    -0x1(%r12),%ebp
and    %r12d,%ebp
mov    (%rbx,%r8,8),%r13
and    %r14,%r13
mov    %ebp,%r12d
cmp    %rcx,%r13
jne    <inf_store::index::Index<M>::position_of+OFF>
jmp    <inf_store::index::Index<M>::position_of+OFF>
nopl   0x0(%rax)
pcmpeqb %xmm1,%xmm2
pmovmskb %xmm2,%edi
test   %edi,%edi
jne    <inf_store::index::Index<M>::position_of+OFF>
inc    %r15
cmp    %r9,%r15
ja     <inf_store::index::Index<M>::position_of+OFF>
add    %r15,%r10
and    %r9,%r10
mov    %r10,%rdi
shl    $0x4,%rdi
lea    0xf(%rdi),%r8
cmp    %rdx,%r8
jb     <inf_store::index::Index<M>::position_of+OFF>
lea    0x10(%rdi),%rsi
lea    0x0(%rip),%rcx        # <anon.c3ede78c9b5129e1230e526cdf53562a.388.llvm.7438497109383721384>
call   *0x0(%rip)        # <_DYNAMIC+OFF>
mov    $0x1,%eax
mov    %r8,%rdx
add    $0x8,%rsp
pop    %rbx
pop    %r12
pop    %r13
pop    %r14
pop    %r15
pop    %rbp
ret
jmp    <inf_store::index::Index<M>::position_of+OFF>
lea    0x0(%rip),%rdx        # <anon.c3ede78c9b5129e1230e526cdf53562a.388.llvm.7438497109383721384+OFF>
mov    %r8,%rdi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
int3
int3
int3
int3
int3
int3
int3
int3
int3
int3
int3
int3

<inf_store::index::Index<M>::insert>:
push   %rbx
mov    0x20(%rdi),%r9
shr    $0x4,%r9
dec    %r9
mov    0x8(%rdi),%r8
mov    %r9,%r10
and    %rsi,%r10
mov    %r10,%rax
shl    $0x4,%rax
lea    0xf(%rax),%rcx
cmp    %r8,%rcx
jae    <inf_store::index::Index<M>::insert+OFF>
mov    (%rdi),%rcx
mov    $0x1,%r11d
nop
movdqu (%rcx,%rax,1),%xmm0
pmovmskb %xmm0,%ebx
test   %ebx,%ebx
jne    <inf_store::index::Index<M>::insert+OFF>
cmp    %r9,%r11
ja     <inf_store::index::Index<M>::insert+OFF>
add    %r11,%r10
and    %r9,%r10
mov    %r10,%rax
shl    $0x4,%rax
lea    0xf(%rax),%rbx
inc    %r11
cmp    %r8,%rbx
jb     <inf_store::index::Index<M>::insert+OFF>
lea    0x10(%rax),%rsi
lea    0x0(%rip),%rcx        # <anon.c3ede78c9b5129e1230e526cdf53562a.388.llvm.7438497109383721384>
mov    %rax,%rdi
mov    %r8,%rdx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
tzcnt  %ebx,%r9d
or     %r9,%rax
cmp    %r8,%rax
jae    <inf_store::index::Index<M>::insert+OFF>
cmpb   $0xfe,(%rcx,%rax,1)
jne    <inf_store::index::Index<M>::insert+OFF>
decq   0x30(%rdi)
mov    %rsi,%r8
shr    $0x39,%r8
mov    %r8b,(%rcx,%rax,1)
mov    0x18(%rdi),%rcx
cmp    %rcx,%rax
jae    <inf_store::index::Index<M>::insert+OFF>
mov    0x10(%rdi),%rcx
shl    $0x6,%rsi
movabs $0x7fff000000000000,%r8
and    %rsi,%r8
movabs $0x8000000000000000,%rsi
or     %rsi,%rdx
or     %r8,%rdx
mov    %rdx,(%rcx,%rax,8)
incq   0x28(%rdi)
pop    %rbx
ret
lea    0x0(%rip),%rdi        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.322.llvm.11790107869987619155+OFF>
lea    0x0(%rip),%rdx        # <anon.c3ede78c9b5129e1230e526cdf53562a.388.llvm.7438497109383721384+OFF>
mov    $0x5d,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.c3ede78c9b5129e1230e526cdf53562a.388.llvm.7438497109383721384+OFF>
mov    %rax,%rdi
mov    %r8,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.c3ede78c9b5129e1230e526cdf53562a.388.llvm.7438497109383721384+OFF>
mov    %rax,%rdi
mov    %rcx,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
int3

<inf_store::index::Index<M>::remove>:
push   %rbx
mov    %rdi,%rbx
call   <inf_store::index::Index<M>::position_of>
cmp    $0x1,%rax
jne    <inf_store::index::Index<M>::remove+OFF>
mov    %rdx,%rdi
and    $0xfffffffffffffff0,%rdi
mov    0x8(%rbx),%rax
mov    %rdx,%rcx
or     $0xf,%rcx
cmp    %rax,%rcx
jae    <inf_store::index::Index<M>::remove+OFF>
mov    (%rbx),%rax
movdqu (%rax,%rdi,1),%xmm0
pcmpeqb 0x0(%rip),%xmm0        # <anon.58baeeae7d5fe476157d296c9f08f803.346.llvm.5844774155465566772+OFF>
pmovmskb %xmm0,%esi
mov    $0x80,%cl
test   %esi,%esi
jne    <inf_store::index::Index<M>::remove+OFF>
incq   0x30(%rbx)
mov    $0xfe,%cl
mov    %cl,(%rax,%rdx,1)
mov    0x18(%rbx),%rsi
cmp    %rsi,%rdx
jae    <inf_store::index::Index<M>::remove+OFF>
mov    0x10(%rbx),%rax
movq   $0x0,(%rax,%rdx,8)
decq   0x28(%rbx)
pop    %rbx
ret
lea    0x10(%rdi),%rsi
lea    0x0(%rip),%rcx        # <anon.c3ede78c9b5129e1230e526cdf53562a.388.llvm.7438497109383721384>
mov    %rax,%rdx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdi        # <anon.c3ede78c9b5129e1230e526cdf53562a.399.llvm.7438497109383721384>
lea    0x0(%rip),%rdx        # <anon.c3ede78c9b5129e1230e526cdf53562a.400.llvm.7438497109383721384>
mov    $0x15,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rax        # <anon.c3ede78c9b5129e1230e526cdf53562a.408.llvm.7438497109383721384>
mov    %rdx,%rdi
mov    %rax,%rdx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
int3
int3
int3
int3
int3
int3
int3
int3
int3
int3
int3
int3

<inf_store::store::CellStore::free_record>:
push   %rbp
push   %r15
push   %r14
push   %r13
push   %r12
push   %rbx
sub    $0x48,%rsp
mov    %rcx,%r14
mov    %rdx,%r15
mov    %rsi,%r12
mov    %rdi,%rbx
cmpb   $0x1,0x2728(%rdi)
jne    <inf_store::store::CellStore::free_record+OFF>
cmpb   $0x1,0x2722(%rbx)
jne    <inf_store::store::CellStore::free_record+OFF>
mov    0x22d0(%rbx),%rcx
mov    0x22d8(%rbx),%rax
movabs $0xffffffffffffff8,%rdx
and    %rax,%rdx
xor    %esi,%esi
data16 data16 data16 data16 data16 cs nopw 0x0(%rax,%rax,1)
cmp    %rsi,%rdx
je     <inf_store::store::CellStore::free_record+OFF>
cmp    %r12,(%rcx,%rsi,8)
je     <inf_store::store::CellStore::free_record+OFF>
cmp    %r12,0x8(%rcx,%rsi,8)
je     <inf_store::store::CellStore::free_record+OFF>
cmp    %r12,0x10(%rcx,%rsi,8)
je     <inf_store::store::CellStore::free_record+OFF>
cmp    %r12,0x18(%rcx,%rsi,8)
je     <inf_store::store::CellStore::free_record+OFF>
cmp    %r12,0x20(%rcx,%rsi,8)
je     <inf_store::store::CellStore::free_record+OFF>
cmp    %r12,0x28(%rcx,%rsi,8)
je     <inf_store::store::CellStore::free_record+OFF>
cmp    %r12,0x30(%rcx,%rsi,8)
je     <inf_store::store::CellStore::free_record+OFF>
cmp    %r12,0x38(%rcx,%rsi,8)
lea    0x8(%rsi),%rsi
jne    <inf_store::store::CellStore::free_record+OFF>
jmp    <inf_store::store::CellStore::free_record+OFF>
shl    $0x3,%eax
and    $0x38,%eax
je     <inf_store::store::CellStore::free_record+OFF>
cmp    %r12,(%rcx,%rdx,8)
je     <inf_store::store::CellStore::free_record+OFF>
cmp    $0x8,%eax
jne    <inf_store::store::CellStore::free_record+OFF>
mov    0x208(%rbx),%ebp
lea    0x80(%rbx),%rcx
mov    0x38(%rbx),%rsi
mov    0x40(%rbx),%rdx
lea    0x8(%rsp),%rdi
mov    %r15,%r8
mov    %r14,%r9
call   <inf_store::doc::doc_root_at>
cmpb   $0x7,0x8(%rsp)
je     <inf_store::store::CellStore::free_record+OFF>
lea    0x2268(%rbx),%rdi
lea    0x8(%rsp),%rdx
mov    %r12,%rsi
mov    %ebp,%ecx
call   <inf_store::index_maint::imp::CellIndexes::remove_doc_entries>
mov    0x38(%rbx),%rsi
mov    0x40(%rbx),%rdx
lea    0x28(%rsp),%r13
mov    %r13,%rdi
mov    %r15,%rcx
mov    %r14,%r8
call   <inf_store::doc::payload_of>
lea    0x2758(%rbx),%rdi
mov    %r12,%rsi
mov    %r15,%rdx
call   <inf_store::index::Index<M>::remove>
mov    %rbx,%rdi
mov    %r15,%rsi
mov    %r14,%rdx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
sub    $0xffffffffffffff80,%rbx
mov    %rbx,%rdi
mov    %r13,%rsi
call   <inf_store::doc::DocStore::release>
add    $0x48,%rsp
pop    %rbx
pop    %r12
pop    %r13
pop    %r14
pop    %r15
pop    %rbp
ret
cmp    %r12,0x8(%rcx,%rdx,8)
je     <inf_store::store::CellStore::free_record+OFF>
cmp    $0x10,%eax
je     <inf_store::store::CellStore::free_record+OFF>
cmp    %r12,0x10(%rcx,%rdx,8)
je     <inf_store::store::CellStore::free_record+OFF>
cmp    $0x18,%eax
je     <inf_store::store::CellStore::free_record+OFF>
cmp    %r12,0x18(%rcx,%rdx,8)
je     <inf_store::store::CellStore::free_record+OFF>
cmp    $0x20,%eax
je     <inf_store::store::CellStore::free_record+OFF>
cmp    %r12,0x20(%rcx,%rdx,8)
je     <inf_store::store::CellStore::free_record+OFF>
cmp    $0x28,%eax
je     <inf_store::store::CellStore::free_record+OFF>
cmp    %r12,0x28(%rcx,%rdx,8)
je     <inf_store::store::CellStore::free_record+OFF>
cmp    $0x30,%eax
je     <inf_store::store::CellStore::free_record+OFF>
cmp    %r12,0x30(%rcx,%rdx,8)
setne  %cl
cmp    $0x38,%rax
sete   %al
test   %al,%cl
jne    <inf_store::store::CellStore::free_record+OFF>
jmp    <inf_store::store::CellStore::free_record+OFF>
int3
int3
int3
int3
int3
int3
int3

<inf_store::store::CellStore::probe_groups>:
push   %rbp
push   %r15
push   %r14
push   %r13
push   %r12
push   %rbx
sub    $0x78,%rsp
mov    %rdx,%rax
mov    %rdi,%r15
movabs $0x1af1d8a50db5eed1,%rdx
mov    %rsi,0x40(%rsp)
mov    %rsi,%rdi
mov    %rax,0x28(%rsp)
mov    %rax,%rsi
call   <inf_foundation::hash::hash64>
mov    0x2760(%r15),%rdx
mov    0x2778(%r15),%rcx
shr    $0x4,%rcx
dec    %rcx
mov    %rcx,0x18(%rsp)
mov    %rcx,%rsi
and    %rax,%rsi
mov    %rsi,%r12
shl    $0x4,%r12
lea    0xf(%r12),%rcx
mov    %rdx,0x10(%rsp)
cmp    %rdx,%rcx
jae    <inf_store::store::CellStore::probe_groups+OFF>
mov    %rax,%r14
shr    $0x2a,%r14
shr    $0x39,%rax
mov    0x2758(%r15),%rcx
mov    %rcx,0x30(%rsp)
movd   %eax,%xmm0
punpcklbw %xmm0,%xmm0
pshuflw $0x0,%xmm0,%xmm0
pshufd $0x44,%xmm0,%xmm0
movdqa %xmm0,0x50(%rsp)
mov    0x2770(%r15),%r13
mov    0x2768(%r15),%rbp
mov    0x38(%r15),%rbx
mov    0x40(%r15),%r11
mov    $0x1,%ecx
movq   $0x0,0x8(%rsp)
mov    0x28(%rsp),%rdx
data16 cs nopw 0x0(%rax,%rax,1)
mov    %rcx,0x20(%rsp)
mov    %rsi,0x38(%rsp)
mov    0x30(%rsp),%rax
movdqu (%rax,%r12,1),%xmm1
movdqa 0x50(%rsp),%xmm0
movdqa %xmm1,0x60(%rsp)
pcmpeqb %xmm1,%xmm0
pmovmskb %xmm0,%eax
test   %eax,%eax
mov    %r12,0x48(%rsp)
jne    <inf_store::store::CellStore::probe_groups+OFF>
mov    0x8(%rsp),%rax
cmp    0x18(%rsp),%rax
jae    <inf_store::store::CellStore::probe_groups+OFF>
movdqa 0x60(%rsp),%xmm0
pcmpeqb 0x0(%rip),%xmm0        # <anon.58baeeae7d5fe476157d296c9f08f803.346.llvm.5844774155465566772+OFF>
pmovmskb %xmm0,%eax
test   %ax,%ax
jne    <inf_store::store::CellStore::probe_groups+OFF>
mov    0x20(%rsp),%rcx
inc    %rcx
mov    0x38(%rsp),%rsi
mov    0x8(%rsp),%rax
add    %rax,%rsi
inc    %rsi
inc    %rax
mov    %rax,0x8(%rsp)
and    0x18(%rsp),%rsi
mov    %rsi,%r12
shl    $0x4,%r12
lea    0xf(%r12),%rax
cmp    0x10(%rsp),%rax
jb     <inf_store::store::CellStore::probe_groups+OFF>
jmp    <inf_store::store::CellStore::probe_groups+OFF>
xchg   %ax,%ax
lea    -0x1(%r15),%eax
and    %r15d,%eax
test   %ax,%ax
je     <inf_store::store::CellStore::probe_groups+OFF>
mov    %eax,%r15d
tzcnt  %r15d,%edi
or     %r12,%rdi
cmp    %r13,%rdi
jae    <inf_store::store::CellStore::probe_groups+OFF>
mov    0x0(%rbp,%rdi,8),%rax
mov    %rax,%rcx
shr    $0x30,%rcx
xor    %r14d,%ecx
test   $0x7fff,%ecx
jne    <inf_store::store::CellStore::probe_groups+OFF>
mov    %rax,%rdi
shr    $0x15,%rdi
and    $0x7ffffff,%edi
cmp    %r11,%rdi
jae    <inf_store::store::CellStore::probe_groups+OFF>
shl    $0x4,%edi
and    $0x1fffff,%eax
lea    0x8(%rax),%rsi
mov    0x8(%rbx,%rdi,1),%rcx
cmp    %rcx,%rsi
ja     <inf_store::store::CellStore::probe_groups+OFF>
add    %rbx,%rdi
mov    (%rdi),%rdi
movzbl 0x1(%rdi,%rax,1),%r10d
movzwl 0x2(%rdi,%rax,1),%esi
movzbl 0x4(%rdi,%rax,1),%r8d
shl    $0x10,%r8d
or     %rsi,%r8
movzbl (%rdi,%rax,1),%esi
and    $0x1,%esi
lea    (%rsi,%rsi,4),%rsi
add    $0x8,%rsi
lea    (%rax,%r10,1),%r9
add    %rsi,%r9
add    %r8,%r9
cmp    %rcx,%r9
ja     <inf_store::store::CellStore::probe_groups+OFF>
cmp    %r10,%rdx
jne    <inf_store::store::CellStore::probe_groups+OFF>
add    %rax,%rdi
add    %rsi,%rdi
mov    0x40(%rsp),%rsi
mov    %rbp,%r12
mov    %r13,%rbp
mov    %r14,%r13
mov    %rbx,%r14
mov    %r11,%rbx
call   *0x0(%rip)        # <bcmp@GLIBC_2.2.5>
mov    %rbx,%r11
mov    %r14,%rbx
mov    %r13,%r14
mov    %rbp,%r13
mov    %r12,%rbp
mov    0x48(%rsp),%r12
mov    0x28(%rsp),%rdx
test   %eax,%eax
jne    <inf_store::store::CellStore::probe_groups+OFF>
mov    0x20(%rsp),%rax
add    $0x78,%rsp
pop    %rbx
pop    %r12
pop    %r13
pop    %r14
pop    %r15
pop    %rbp
ret
lea    0x0(%rip),%rdi        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.322.llvm.11790107869987619155+OFF>
lea    0x0(%rip),%rdx        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.323.llvm.11790107869987619155+OFF>
mov    $0x63,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x10(%r12),%rsi
lea    0x0(%rip),%rcx        # <anon.c3ede78c9b5129e1230e526cdf53562a.388.llvm.7438497109383721384>
mov    %r12,%rdi
mov    0x10(%rsp),%rdx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.c3ede78c9b5129e1230e526cdf53562a.388.llvm.7438497109383721384+OFF>
mov    %r13,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.323.llvm.11790107869987619155+OFF>
mov    %r11,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
int3
int3
int3
int3
int3
int3
int3
int3
int3
int3
int3
int3

<inf_store::store::CellStore::get_with_hash>:
push   %rbx
sub    $0x20,%rsp
mov    %r8,%r9
mov    %rcx,%r8
mov    %rdx,%rcx
mov    %rsi,%rdx
mov    %rdi,%rbx
lea    0x8(%rsp),%rdi
mov    %rbx,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
cmpl   $0x1,0x8(%rsp)
jne    <inf_store::store::CellStore::get_with_hash+OFF>
mov    0x10(%rsp),%rax
mov    0x18(%rsp),%rdx
incq   0x2790(%rbx)
mov    0x40(%rbx),%rsi
mov    %rax,%rdi
shr    $0x15,%rdi
cmp    %rsi,%rdi
jae    <inf_store::store::CellStore::get_with_hash+OFF>
mov    0x38(%rbx),%rcx
shl    $0x4,%rdi
and    $0x1fffff,%eax
lea    (%rax,%rdx,1),%rsi
cmp    0x8(%rcx,%rdi,1),%rsi
ja     <inf_store::store::CellStore::get_with_hash+OFF>
cmp    $0x1,%rdx
je     <inf_store::store::CellStore::get_with_hash+OFF>
test   %rdx,%rdx
je     <inf_store::store::CellStore::get_with_hash+OFF>
cmp    $0x4,%rdx
jbe    <inf_store::store::CellStore::get_with_hash+OFF>
add    %rdi,%rcx
add    (%rcx),%rax
movzbl (%rax),%ecx
and    $0x1,%ecx
lea    (%rcx,%rcx,4),%rcx
movzbl 0x1(%rax),%esi
lea    (%rsi,%rcx,1),%rdi
add    $0x8,%rdi
movzwl 0x2(%rax),%esi
movzbl 0x4(%rax),%ecx
shl    $0x10,%ecx
or     %rsi,%rcx
lea    (%rdi,%rcx,1),%rsi
cmp    %rdx,%rsi
ja     <inf_store::store::CellStore::get_with_hash+OFF>
add    %rdi,%rax
mov    %rcx,%rdx
add    $0x20,%rsp
pop    %rbx
ret
incq   0x2798(%rbx)
xor    %eax,%eax
mov    %rcx,%rdx
add    $0x20,%rsp
pop    %rbx
ret
lea    0x0(%rip),%rdi        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.322.llvm.11790107869987619155+OFF>
lea    0x0(%rip),%rdx        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.323.llvm.11790107869987619155+OFF>
mov    $0x63,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rcx        # <anon.c3ede78c9b5129e1230e526cdf53562a.408.llvm.7438497109383721384+OFF>
mov    $0x2,%edi
mov    $0x5,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rcx        # <anon.c3ede78c9b5129e1230e526cdf53562a.408.llvm.7438497109383721384+OFF>
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.323.llvm.11790107869987619155+OFF>
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.323.llvm.11790107869987619155+OFF>
mov    $0x1,%edi
mov    $0x1,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.c3ede78c9b5129e1230e526cdf53562a.408.llvm.7438497109383721384+OFF>
xor    %edi,%edi
xor    %esi,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
int3
int3
int3
int3
int3
int3
int3
int3
int3
int3

<inf_store::store::CellStore::resolve_hashed>:
push   %rbp
push   %r15
push   %r14
push   %r13
push   %r12
push   %rbx
sub    $0xa8,%rsp
mov    %rcx,%r10
mov    %rdx,0x58(%rsp)
mov    %rdi,(%rsp)
mov    0x2760(%rsi),%rcx
mov    0x2778(%rsi),%rdx
shr    $0x4,%rdx
dec    %rdx
mov    %rdx,%rdi
and    %r8,%rdi
mov    %rdi,%r11
shl    $0x4,%r11
lea    0xf(%r11),%rax
mov    %rcx,0x10(%rsp)
cmp    %rcx,%rax
jae    <inf_store::store::CellStore::resolve_hashed+OFF>
mov    %r9,0x18(%rsp)
mov    %r8,%r9
shr    $0x2a,%r9
mov    %r8,0x8(%rsp)
mov    %r8,%rax
shr    $0x39,%rax
mov    0x2758(%rsi),%rcx
mov    %rcx,0x30(%rsp)
movd   %eax,%xmm0
punpcklbw %xmm0,%xmm0
pshuflw $0x0,%xmm0,%xmm0
pshufd $0x44,%xmm0,%xmm0
movdqa %xmm0,0x80(%rsp)
mov    0x2770(%rsi),%r14
mov    0x2768(%rsi),%rax
mov    %rax,0x78(%rsp)
mov    0x38(%rsi),%rax
mov    %rax,0x70(%rsp)
mov    %rsi,0x20(%rsp)
mov    0x40(%rsi),%rbp
xor    %ecx,%ecx
mov    %rdx,0x28(%rsp)
mov    %r14,0x50(%rsp)
mov    %rbp,0x48(%rsp)
cs nopw 0x0(%rax,%rax,1)
mov    %rcx,0x40(%rsp)
mov    %rdi,0x38(%rsp)
mov    0x30(%rsp),%rax
movdqu (%rax,%r11,1),%xmm1
movdqa 0x80(%rsp),%xmm0
movdqa %xmm1,0x90(%rsp)
pcmpeqb %xmm1,%xmm0
pmovmskb %xmm0,%eax
test   %eax,%eax
mov    %r11,0x60(%rsp)
jne    <inf_store::store::CellStore::resolve_hashed+OFF>
movdqa 0x90(%rsp),%xmm0
pcmpeqb 0x0(%rip),%xmm0        # <anon.58baeeae7d5fe476157d296c9f08f803.346.llvm.5844774155465566772+OFF>
pmovmskb %xmm0,%eax
test   %eax,%eax
jne    <inf_store::store::CellStore::resolve_hashed+OFF>
mov    0x40(%rsp),%rcx
inc    %rcx
mov    0x28(%rsp),%rdx
cmp    %rdx,%rcx
ja     <inf_store::store::CellStore::resolve_hashed+OFF>
mov    0x38(%rsp),%rdi
add    %rcx,%rdi
and    %rdx,%rdi
mov    %rdi,%r11
shl    $0x4,%r11
lea    0xf(%r11),%rax
cmp    0x10(%rsp),%rax
jb     <inf_store::store::CellStore::resolve_hashed+OFF>
jmp    <inf_store::store::CellStore::resolve_hashed+OFF>
data16 data16 data16 data16 data16 cs nopw 0x0(%rax,%rax,1)
lea    -0x1(%rbx),%eax
and    %ebx,%eax
test   %ax,%ax
je     <inf_store::store::CellStore::resolve_hashed+OFF>
mov    %eax,%ebx
tzcnt  %ebx,%edi
or     %r11,%rdi
cmp    %r14,%rdi
jae    <inf_store::store::CellStore::resolve_hashed+OFF>
mov    0x78(%rsp),%rax
mov    (%rax,%rdi,8),%rax
mov    %rax,%rcx
shr    $0x30,%rcx
xor    %r9d,%ecx
test   $0x7fff,%ecx
jne    <inf_store::store::CellStore::resolve_hashed+OFF>
mov    %rax,%r8
movabs $0xffffffffffff,%rcx
and    %rcx,%r8
mov    %r8,%rdi
shr    $0x15,%rdi
cmp    %rbp,%rdi
jae    <inf_store::store::CellStore::resolve_hashed+OFF>
shl    $0x4,%edi
and    $0x1fffff,%eax
lea    0x8(%rax),%rdx
mov    0x70(%rsp),%rsi
mov    0x8(%rsi,%rdi,1),%rcx
cmp    %rcx,%rdx
ja     <inf_store::store::CellStore::resolve_hashed+OFF>
add    %rsi,%rdi
mov    (%rdi),%r12
movzbl (%r12,%rax,1),%edi
movzbl 0x1(%r12,%rax,1),%edx
movzwl 0x2(%r12,%rax,1),%esi
movzbl 0x4(%r12,%rax,1),%r13d
shl    $0x10,%r13d
or     %rsi,%r13
mov    %rdi,%r15
mov    %edi,%esi
and    $0x1,%esi
lea    (%rsi,%rsi,4),%rdi
add    $0x8,%rdi
add    %rdi,%r13
lea    (%rax,%rdx,1),%rsi
add    %r13,%rsi
cmp    %rcx,%rsi
ja     <inf_store::store::CellStore::resolve_hashed+OFF>
cmp    %rdx,%r10
jne    <inf_store::store::CellStore::resolve_hashed+OFF>
add    %rax,%r12
add    %r12,%rdi
mov    0x58(%rsp),%rsi
mov    %r10,%rdx
mov    %r10,%rbp
mov    %r9,%r14
mov    %r8,0x68(%rsp)
call   *0x0(%rip)        # <bcmp@GLIBC_2.2.5>
mov    0x60(%rsp),%r11
mov    0x68(%rsp),%r8
mov    %r14,%r9
mov    0x50(%rsp),%r14
mov    %rbp,%r10
mov    0x48(%rsp),%rbp
test   %eax,%eax
jne    <inf_store::store::CellStore::resolve_hashed+OFF>
add    %r10,%r13
test   $0x1,%r15b
mov    0x20(%rsp),%rbx
je     <inf_store::store::CellStore::resolve_hashed+OFF>
cmp    $0xc,%r13
jbe    <inf_store::store::CellStore::resolve_hashed+OFF>
mov    0x8(%r12),%ecx
movabs $0x431bde82d7b634db,%rdx
mov    0x18(%rsp),%rax
mul    %rdx
movzbl 0xc(%r12),%eax
shl    $0x20,%rax
or     %rcx,%rax
shr    $0x12,%rdx
cmp    %rax,%rdx
jae    <inf_store::store::CellStore::resolve_hashed+OFF>
movzbl 0x2751(%rbx),%eax
test   %eax,%eax
je     <inf_store::store::CellStore::resolve_hashed+OFF>
cmp    $0x1,%eax
jne    <inf_store::store::CellStore::resolve_hashed+OFF>
or     $0xc,%r15b
mov    %r15b,(%r12)
jmp    <inf_store::store::CellStore::resolve_hashed+OFF>
mov    (%rsp),%rax
xor    %ecx,%ecx
jmp    <inf_store::store::CellStore::resolve_hashed+OFF>
movabs $0x9e3779b97f4a7c15,%rax
add    0x2740(%rbx),%rax
mov    %rax,0x2740(%rbx)
mov    0x2738(%rbx),%rcx
test   %rcx,%rcx
je     <inf_store::store::CellStore::resolve_hashed+OFF>
mov    0x8(%rsp),%rdx
mov    %edx,%r15d
shr    $0xb,%r15d
and    $0x7ff,%r15d
mov    %rdx,%rdi
shr    $0x16,%rdi
and    $0x7ff,%edi
mov    %rdx,%rsi
shr    $0x21,%rsi
and    $0x7ff,%esi
and    $0x7ff,%edx
movzbl (%rcx,%rdx,1),%r14d
movzbl 0x800(%rcx,%r15,1),%ebx
movzbl 0x1000(%rcx,%rdi,1),%r9d
cmp    %bl,%r14b
mov    %ebx,%ebp
cmovb  %r14d,%ebp
cmp    %r9b,%bpl
cmovae %r9d,%ebp
mov    %ebp,%r11d
movzbl 0x1800(%rcx,%rsi,1),%r10d
cmp    %r10b,%bpl
cmovae %r10d,%ebp
cmp    $0xff,%bpl
je     <inf_store::store::CellStore::resolve_hashed+OFF>
mov    %rdx,%r12
mov    %rax,%rdx
shr    $0x1e,%rdx
xor    %rax,%rdx
movabs $0xbf58476d1ce4e5b9,%rax
imul   %rdx,%rax
mov    %rax,%rdx
shr    $0x1b,%rdx
xor    %rax,%rdx
movabs $0x94d049bb133111eb,%rax
imul   %rdx,%rax
mov    %rax,%rdx
shr    $0x1f,%rdx
xor    %rax,%rdx
movzbl %bpl,%eax
add    %eax,%eax
lea    (%rax,%rax,4),%rax
inc    %rax
mul    %rdx
jo     <inf_store::store::CellStore::resolve_hashed+OFF>
cmp    %bpl,%r14b
je     <inf_store::store::CellStore::resolve_hashed+OFF>
cmp    %bpl,%bl
je     <inf_store::store::CellStore::resolve_hashed+OFF>
cmp    %bpl,%r9b
je     <inf_store::store::CellStore::resolve_hashed+OFF>
cmp    %r11b,%r10b
ja     <inf_store::store::CellStore::resolve_hashed+OFF>
lea    0x1(%r10),%eax
mov    %al,0x1800(%rcx,%rsi,1)
mov    (%rsp),%rax
mov    %r8,0x8(%rax)
mov    %r13,0x10(%rax)
mov    $0x1,%ecx
jmp    <inf_store::store::CellStore::resolve_hashed+OFF>
mov    %rbx,%rdi
mov    0x8(%rsp),%rsi
mov    %r8,%rdx
mov    %r13,%rcx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
incq   0x27a0(%rbx)
mov    0x27b0(%rbx),%rax
cmp    $0x1,%rax
adc    $0xffffffffffffffff,%rax
mov    %rax,0x27b0(%rbx)
xor    %ecx,%ecx
mov    (%rsp),%rax
mov    %rcx,(%rax)
add    $0xa8,%rsp
pop    %rbx
pop    %r12
pop    %r13
pop    %r14
pop    %r15
pop    %rbp
ret
lea    0x0(%rip),%rdi        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.322.llvm.11790107869987619155+OFF>
lea    0x0(%rip),%rdx        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.323.llvm.11790107869987619155+OFF>
mov    $0x63,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x1(%r14),%eax
mov    %al,(%rcx,%r12,1)
cmp    %bpl,%bl
jne    <inf_store::store::CellStore::resolve_hashed+OFF>
lea    0x1(%rbx),%eax
mov    %al,0x800(%rcx,%r15,1)
cmp    %bpl,%r9b
jne    <inf_store::store::CellStore::resolve_hashed+OFF>
lea    0x1(%r9),%eax
mov    %al,0x1000(%rcx,%rdi,1)
cmp    %r11b,%r10b
jbe    <inf_store::store::CellStore::resolve_hashed+OFF>
jmp    <inf_store::store::CellStore::resolve_hashed+OFF>
lea    0x10(%r11),%rsi
lea    0x0(%rip),%rcx        # <anon.c3ede78c9b5129e1230e526cdf53562a.388.llvm.7438497109383721384>
mov    %r11,%rdi
mov    0x10(%rsp),%rdx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rcx        # <anon.c3ede78c9b5129e1230e526cdf53562a.408.llvm.7438497109383721384+OFF>
mov    $0x8,%edi
mov    $0xd,%esi
mov    %r13,%rdx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.c3ede78c9b5129e1230e526cdf53562a.388.llvm.7438497109383721384+OFF>
mov    %r14,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.323.llvm.11790107869987619155+OFF>
mov    %rbp,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
int3
int3
int3
int3
int3
int3

<inf_store::store::CellStore::write_record_carrying>:
push   %rbp
push   %r15
push   %r14
push   %r13
push   %r12
push   %rbx
sub    $0x88,%rsp
mov    %r9,%r12
mov    %r8,%rbx
mov    %rdx,%rax
mov    %rsi,%r15
mov    %rdi,%r13
mov    (%r9),%rdx
mov    0x28(%r9),%rsi
lea    (%rdx,%rdx,4),%rdx
add    0x18(%r9),%rdx
lea    (%rsi,%rdx,1),%r14
add    $0x8,%r14
movabs $0x1af1d8a50db5eed1,%rdx
mov    %rax,%rdi
mov    %rcx,%rsi
call   <inf_foundation::hash::hash64>
mov    %rax,0x20(%rsp)
cmpl   $0x1,(%rbx)
jne    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x8(%rbx),%rbp
mov    0x10(%rbx),%rbx
mov    %r15,%rdi
mov    %rbp,%rsi
mov    %rbx,%rdx
mov    %r14,%rcx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
test   %al,%al
mov    %r13,0x10(%rsp)
je     <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x40(%r15),%rsi
mov    %rbp,%rdi
shr    $0x15,%rdi
cmp    %rsi,%rdi
jae    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x38(%r15),%rax
shl    $0x4,%rdi
mov    %ebp,%esi
and    $0x1fffff,%esi
lea    (%rsi,%r14,1),%rcx
cmp    0x8(%rax,%rdi,1),%rcx
ja     <inf_store::store::CellStore::write_record_carrying+OFF>
add    %rdi,%rax
add    (%rax),%rsi
mov    %r12,%rdi
mov    %r14,%rdx
call   <inf_store::record::RecordSpec::write>
mov    0x20(%rsp),%r12
movzbl 0x2751(%r15),%eax
mov    $0xb,%ecx
test   %eax,%eax
jne    <inf_store::store::CellStore::write_record_carrying+OFF>
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    %r14,0x38(%rsp)
lea    0x2758(%r15),%rbx
mov    0x2778(%r15),%r14
mov    0x2780(%r15),%rax
mov    0x2788(%r15),%rcx
lea    (%rcx,%rax,1),%rdx
imul   $0x64,%rdx,%rdx
add    $0x64,%rdx
imul   $0x55,%r14,%rsi
cmp    %rsi,%rdx
jbe    <inf_store::store::CellStore::write_record_carrying+OFF>
cmp    %rax,%rcx
setb   %cl
mov    %r14,%rbx
shl    %cl,%rbx
test   %rbx,%rbx
jns    <inf_store::store::CellStore::write_record_carrying+OFF>
xor    %edi,%edi
mov    %rbx,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
mov    %r15,%rdi
mov    %r14,%r13
mov    %r14,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
mov    $0x6,%ecx
test   $0x1,%al
je     <inf_store::store::CellStore::write_record_carrying+OFF>
mov    %rdx,%r14
mov    0x40(%r15),%rsi
mov    %rdx,%rdi
shr    $0x15,%rdi
cmp    %rsi,%rdi
jae    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x38(%r15),%rax
shl    $0x4,%rdi
mov    %r14d,%esi
and    $0x1fffff,%esi
lea    (%rsi,%r13,1),%rcx
cmp    0x8(%rax,%rdi,1),%rcx
ja     <inf_store::store::CellStore::write_record_carrying+OFF>
add    %rdi,%rax
add    (%rax),%rsi
mov    %r12,%rdi
mov    %r13,%rdx
call   <inf_store::record::RecordSpec::write>
lea    0x2758(%r15),%rdi
mov    0x20(%rsp),%r12
mov    %r12,%rsi
mov    %rbp,%rdx
call   <inf_store::index::Index<M>::position_of>
cmp    $0x1,%rax
jne    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x2770(%r15),%rsi
cmp    %rsi,%rdx
jae    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x2768(%r15),%rax
mov    %r12,%rcx
shl    $0x6,%rcx
movabs $0x7fff000000000000,%rsi
and    %rcx,%rsi
movabs $0x7ffffffffffffff8,%rcx
add    $0x8,%rcx
or     %rsi,%rcx
or     %r14,%rcx
mov    %rcx,(%rax,%rdx,8)
mov    %r15,%rdi
mov    %rbp,%rsi
mov    %rbx,%rdx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
mov    %r14,%rbp
movzbl 0x2751(%r15),%eax
mov    $0xb,%ecx
test   %eax,%eax
jne    <inf_store::store::CellStore::write_record_carrying+OFF>
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
je     <inf_store::store::CellStore::write_record_carrying+OFF>
mov    %rbx,%rdi
call   *0x0(%rip)        # <malloc@GLIBC_2.2.5>
mov    %rax,%rbp
test   %rax,%rax
jne    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    $0x1,%edi
mov    %rbx,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
mov    $0x1,%ebp
mov    %rbp,%rdi
mov    $0x80,%esi
mov    %rbx,%rdx
call   *0x0(%rip)        # <memset@GLIBC_2.2.5>
lea    0x0(,%rbx,8),%rsi
mov    %rbx,%rax
shr    $0x3d,%rax
setne  %al
movabs $0x7ffffffffffffff8,%rcx
cmp    %rcx,%rsi
seta   %cl
or     %al,%cl
je     <inf_store::store::CellStore::write_record_carrying+OFF>
xor    %edi,%edi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    %rbp,0x8(%rsp)
test   %rsi,%rsi
je     <inf_store::store::CellStore::write_record_carrying+OFF>
mov    %rsi,%rdi
call   *0x0(%rip)        # <malloc@GLIBC_2.2.5>
mov    %rax,%rbp
test   %rax,%rax
je     <inf_store::store::CellStore::write_record_carrying+OFF>
mov    %r13,0x10(%rsp)
cmp    $0x2,%rbx
jb     <inf_store::store::CellStore::write_record_carrying+OFF>
lea    0x0(,%rbx,8),%r13
lea    -0x8(%r13),%rdx
mov    %rbp,%rdi
xor    %esi,%esi
call   *0x0(%rip)        # <memset@GLIBC_2.2.5>
lea    -0x8(,%rbp,1),%rax
add    %r13,%rax
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    $0x8,%ebp
mov    %r13,0x10(%rsp)
cmp    $0x2,%rbx
jae    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    %rbp,%rax
test   %rbx,%rbx
je     <inf_store::store::CellStore::write_record_carrying+OFF>
movq   $0x0,(%rax)
mov    0x8(%rsp),%rax
mov    %rax,0x50(%rsp)
mov    %rbx,0x58(%rsp)
mov    %rbp,0x60(%rsp)
mov    %rbx,0x68(%rsp)
mov    %rbx,0x70(%rsp)
xorps  %xmm0,%xmm0
movups %xmm0,0x78(%rsp)
mov    0x2760(%r15),%rax
mov    %rax,0x8(%rsp)
test   %r14,%r14
je     <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x2758(%r15),%rax
mov    %rax,0x48(%rsp)
mov    0x2770(%r15),%rax
mov    %rax,0x18(%rsp)
mov    0x2768(%r15),%rax
mov    %rax,0x40(%rsp)
mov    0x38(%r15),%rbp
mov    0x40(%r15),%rax
mov    %rax,0x30(%rsp)
xor    %r13d,%r13d
lea    0x0(%rip),%rax        # <anon.c3ede78c9b5129e1230e526cdf53562a.388.llvm.7438497109383721384+OFF>
mov    %rax,0x28(%rsp)
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
cs nopw 0x0(%rax,%rax,1)
inc    %r13
cmp    %r13,%r14
je     <inf_store::store::CellStore::write_record_carrying+OFF>
cmp    %r13,0x8(%rsp)
je     <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x48(%rsp),%rax
cmpb   $0x0,(%rax,%r13,1)
js     <inf_store::store::CellStore::write_record_carrying+OFF>
cmp    0x18(%rsp),%r13
jae    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x40(%rsp),%rax
mov    (%rax,%r13,8),%rax
mov    %rax,%rbx
movabs $0xffffffffffff,%rcx
and    %rcx,%rbx
mov    %rbx,%rdi
shr    $0x15,%rdi
cmp    0x30(%rsp),%rdi
jae    <inf_store::store::CellStore::write_record_carrying+OFF>
shl    $0x4,%edi
and    $0x1fffff,%eax
lea    0x8(%rax),%rdx
mov    0x8(%rbp,%rdi,1),%rcx
cmp    %rcx,%rdx
ja     <inf_store::store::CellStore::write_record_carrying+OFF>
add    %rbp,%rdi
mov    (%rdi),%rdi
movzbl 0x1(%rdi,%rax,1),%esi
movzwl 0x2(%rdi,%rax,1),%edx
movzbl 0x4(%rdi,%rax,1),%r8d
shl    $0x10,%r8d
or     %rdx,%r8
movzbl (%rdi,%rax,1),%edx
and    $0x1,%edx
lea    (%rdx,%rdx,4),%rdx
add    $0x8,%rdx
lea    (%rax,%rsi,1),%r9
add    %rdx,%r9
add    %r8,%r9
cmp    %rcx,%r9
ja     <inf_store::store::CellStore::write_record_carrying+OFF>
add    %rax,%rdi
add    %rdx,%rdi
movabs $0x1af1d8a50db5eed1,%rdx
call   <inf_foundation::hash::hash64>
lea    0x50(%rsp),%rdi
mov    %rax,%rsi
mov    %rbx,%rdx
call   <inf_store::index::Index<M>::insert>
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
cmpq   $0x0,0x8(%rsp)
lea    0x2758(%r15),%rbx
je     <inf_store::store::CellStore::write_record_carrying+OFF>
mov    (%rbx),%rdi
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
cmpq   $0x0,0x2770(%r15)
mov    0x10(%rsp),%r13
je     <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x2768(%r15),%rdi
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
mov    0x80(%rsp),%rax
mov    %rax,0x30(%rbx)
movups 0x50(%rsp),%xmm0
movups 0x60(%rsp),%xmm1
movups 0x70(%rsp),%xmm2
movups %xmm2,0x20(%rbx)
movups %xmm1,0x10(%rbx)
movups %xmm0,(%rbx)
mov    %r13,0x10(%rsp)
mov    %r15,%rdi
mov    0x38(%rsp),%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
mov    $0x6,%ecx
test   $0x1,%al
je     <inf_store::store::CellStore::write_record_carrying+OFF>
mov    %rdx,%rbp
mov    0x40(%r15),%rsi
mov    %rdx,%rdi
shr    $0x15,%rdi
cmp    %rsi,%rdi
jae    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x38(%r15),%rax
shl    $0x4,%rdi
mov    %ebp,%esi
and    $0x1fffff,%esi
mov    0x38(%rsp),%rdx
lea    (%rsi,%rdx,1),%rcx
cmp    0x8(%rax,%rdi,1),%rcx
ja     <inf_store::store::CellStore::write_record_carrying+OFF>
add    %rdi,%rax
add    (%rax),%rsi
mov    %r12,%rdi
call   <inf_store::record::RecordSpec::write>
mov    %rbx,%rdi
mov    0x20(%rsp),%r12
mov    %r12,%rsi
mov    %rbp,%rdx
call   <inf_store::index::Index<M>::insert>
movzbl 0x2751(%r15),%eax
mov    $0xb,%ecx
test   %eax,%eax
je     <inf_store::store::CellStore::write_record_carrying+OFF>
cmp    $0x1,%eax
jne    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x40(%r15),%rsi
mov    %rbp,%rdi
shr    $0x15,%rdi
cmp    %rsi,%rdi
jae    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x38(%r15),%rax
shl    $0x4,%rdi
and    $0x1fffff,%ebp
cmp    0x8(%rax,%rdi,1),%rbp
jae    <inf_store::store::CellStore::write_record_carrying+OFF>
add    %rdi,%rax
mov    (%rax),%rax
movzbl (%rax,%rbp,1),%edx
and    $0xf3,%dl
or     $0x4,%dl
mov    %dl,(%rax,%rbp,1)
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
movabs $0x9e3779b97f4a7c15,%rax
add    0x2740(%r15),%rax
mov    %rax,0x2740(%r15)
mov    0x2738(%r15),%rsi
test   %rsi,%rsi
je     <inf_store::store::CellStore::write_record_carrying+OFF>
mov    %r12d,%r9d
shr    $0xb,%r9d
and    $0x7ff,%r9d
mov    %r12,%r8
shr    $0x16,%r8
and    $0x7ff,%r8d
mov    %r12,%rdi
shr    $0x21,%rdi
and    $0x7ff,%edi
and    $0x7ff,%r12d
movzbl (%rsi,%r12,1),%r15d
movzbl 0x800(%rsi,%r9,1),%r14d
movzbl 0x1000(%rsi,%r8,1),%r10d
cmp    %r14b,%r15b
mov    %r14d,%ebp
cmovb  %r15d,%ebp
cmp    %r10b,%bpl
cmovae %r10d,%ebp
mov    %ebp,%ebx
movzbl 0x1800(%rsi,%rdi,1),%r11d
cmp    %r11b,%bpl
cmovae %r11d,%ebp
cmp    $0xff,%bpl
je     <inf_store::store::CellStore::write_record_carrying+OFF>
mov    %rax,%rdx
shr    $0x1e,%rdx
xor    %rax,%rdx
movabs $0xbf58476d1ce4e5b9,%rax
imul   %rdx,%rax
mov    %rax,%rdx
shr    $0x1b,%rdx
xor    %rax,%rdx
movabs $0x94d049bb133111eb,%rax
imul   %rdx,%rax
mov    %rax,%rdx
shr    $0x1f,%rdx
xor    %rax,%rdx
movzbl %bpl,%eax
add    %eax,%eax
lea    (%rax,%rax,4),%rax
inc    %rax
mul    %rdx
jo     <inf_store::store::CellStore::write_record_carrying+OFF>
cmp    %bpl,%r15b
je     <inf_store::store::CellStore::write_record_carrying+OFF>
cmp    %bpl,%r14b
je     <inf_store::store::CellStore::write_record_carrying+OFF>
cmp    %bpl,%r10b
je     <inf_store::store::CellStore::write_record_carrying+OFF>
cmp    %bl,%r11b
ja     <inf_store::store::CellStore::write_record_carrying+OFF>
lea    0x1(%r11),%eax
mov    %al,0x1800(%rsi,%rdi,1)
mov    0x10(%rsp),%rax
mov    %rcx,(%rax)
add    $0x88,%rsp
pop    %rbx
pop    %r12
pop    %r13
pop    %r14
pop    %r15
pop    %rbp
ret
lea    0x1(%r15),%eax
mov    %al,(%rsi,%r12,1)
cmp    %bpl,%r14b
jne    <inf_store::store::CellStore::write_record_carrying+OFF>
lea    0x1(%r14),%eax
mov    %al,0x800(%rsi,%r9,1)
cmp    %bpl,%r10b
jne    <inf_store::store::CellStore::write_record_carrying+OFF>
lea    0x1(%r10),%eax
mov    %al,0x1000(%rsi,%r8,1)
cmp    %bl,%r11b
jbe    <inf_store::store::CellStore::write_record_carrying+OFF>
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
lea    0x0(%rip),%rdi        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.322.llvm.11790107869987619155+OFF>
lea    0x0(%rip),%rdx        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.323.llvm.11790107869987619155+OFF>
mov    $0x63,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
lea    0x0(%rip),%rdi        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.322.llvm.11790107869987619155+OFF>
lea    0x0(%rip),%rdx        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.323.llvm.11790107869987619155+OFF>
mov    $0x63,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdi        # <anon.c3ede78c9b5129e1230e526cdf53562a.399.llvm.7438497109383721384+OFF>
lea    0x0(%rip),%rdx        # <anon.c3ede78c9b5129e1230e526cdf53562a.400.llvm.7438497109383721384+OFF>
mov    $0x16,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
mov    0x8(%rsp),%rax
mov    %rax,0x18(%rsp)
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
lea    0x0(%rip),%rdx        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.323.llvm.11790107869987619155+OFF>
mov    0x30(%rsp),%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    %r13,0x8(%rsp)
lea    0x0(%rip),%rax        # <anon.c3ede78c9b5129e1230e526cdf53562a.388.llvm.7438497109383721384+OFF>
mov    %rax,0x28(%rsp)
mov    0x8(%rsp),%rdi
mov    0x18(%rsp),%rsi
mov    0x28(%rsp),%rdx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
ud2
lea    0x0(%rip),%rdx        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.323.llvm.11790107869987619155+OFF>
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rax        # <anon.c3ede78c9b5129e1230e526cdf53562a.400.llvm.7438497109383721384+OFF>
mov    %rdx,%rdi
mov    %rax,%rdx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
mov    $0x8,%edi
mov    0x8(%rsp),%rbp
lea    0x0(,%rbx,8),%rsi
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    %rax,%r15
lea    0x50(%rsp),%rdi
call   <core::ptr::drop_in_place<inf_store::index::Index>>
mov    %r15,%rdi
call   <_Unwind_Resume@plt>
mov    %rax,%r15
test   %rbx,%rbx
jne    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    %r15,%rdi
call   <_Unwind_Resume@plt>
mov    %rbp,%rdi
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
mov    %r15,%rdi
call   <_Unwind_Resume@plt>
int3
int3
int3
int3
int3
int3
int3
int3
int3
int3
int3
int3
int3

<inf_store::store::CellStore::new>:
push   %rbp
push   %r15
push   %r14
push   %r13
push   %r12
push   %rbx
sub    $0xd8,%rsp
mov    0x10(%rsi),%r12
lea    -0x1(%r12),%rax
mov    %r12,%rcx
xor    %rax,%rcx
cmp    %rax,%rcx
jbe    <inf_store::store::CellStore::new+OFF>
lea    -0x10000(%r12),%rax
cmp    $0x1f0000,%rax
ja     <inf_store::store::CellStore::new+OFF>
mov    %rsi,%rbp
mov    %rdi,%rbx
mov    0x38(%rsi),%rcx
shr    $0x2,%r12
mov    $0x100,%eax
mov    $0x1c,%r14d
mov    $0x2e8,%r15d
nopl   0x0(%rax)
add    %rax,%rax
add    $0x4,%r14
add    $0x60,%r15
cmp    %r12,%rax
jb     <inf_store::store::CellStore::new+OFF>
movabs $0x555555555555555,%rax
lea    0x3(%r14),%r13
cmp    %rax,%r13
jbe    <inf_store::store::CellStore::new+OFF>
xor    %edi,%edi
mov    %r15,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
mov    %rcx,0x10(%rsp)
mov    %r15,%rdi
call   *0x0(%rip)        # <malloc@GLIBC_2.2.5>
test   %rax,%rax
je     <inf_store::store::CellStore::new+OFF>
xor    %ecx,%ecx
test   %r14,%r14
je     <inf_store::store::CellStore::new+OFF>
mov    0x0(%rip),%rdx        # <anon.75fd87380dfc59308f7611a9a6d5ea54.44.llvm.15107721511710138065+OFF>
movups 0x0(%rip),%xmm0        # <anon.75fd87380dfc59308f7611a9a6d5ea54.44.llvm.15107721511710138065>
mov    %rax,%rsi
nopl   (%rax)
mov    %rdx,0x10(%rsi)
movups %xmm0,(%rsi)
movups %xmm0,0x18(%rsi)
mov    %rdx,0x28(%rsi)
movups %xmm0,0x30(%rsi)
mov    %rdx,0x40(%rsi)
add    $0x4,%rcx
movups %xmm0,0x48(%rsi)
mov    %rdx,0x58(%rsi)
add    $0x60,%rsi
cmp    %rcx,%r14
jne    <inf_store::store::CellStore::new+OFF>
lea    (%rcx,%rcx,2),%rcx
lea    (%rax,%rcx,8),%rcx
mov    $0xffffffffffffffb8,%rdx
mov    0x0(%rip),%r14        # <anon.75fd87380dfc59308f7611a9a6d5ea54.44.llvm.15107721511710138065+OFF>
movups 0x0(%rip),%xmm0        # <anon.75fd87380dfc59308f7611a9a6d5ea54.44.llvm.15107721511710138065>
nopl   0x0(%rax)
mov    %r14,0x58(%rcx,%rdx,1)
movups %xmm0,0x48(%rcx,%rdx,1)
add    $0x18,%rdx
jne    <inf_store::store::CellStore::new+OFF>
mov    0x10(%rbp),%rcx
mov    %rcx,0x60(%rsp)
movups 0x0(%rbp),%xmm0
movaps %xmm0,0x50(%rsp)
mov    %r12,0xb0(%rsp)
mov    %r13,0x68(%rsp)
mov    %rax,0x70(%rsp)
mov    %r13,0x78(%rsp)
movq   $0x0,0x80(%rsp)
movq   $0x8,0x88(%rsp)
xorps  %xmm0,%xmm0
movaps %xmm0,0x90(%rsp)
movq   $0x4,0xa0(%rsp)
movq   $0x0,0xa8(%rsp)
movups %xmm0,0xb8(%rsp)
mov    0x30(%rbp),%rax
cmp    $0x41,%rax
mov    $0x40,%ecx
cmovae %rax,%rcx
imul   $0x64,%rcx,%rax
movabs $0xc0c0c0c0c0c0c0c1,%rdx
mul    %rdx
shr    $0x6,%rdx
movabs $0xd2d2d2d2d2d2d2d4,%rsi
imul   %rcx,%rsi
movabs $0x303030303030303,%rcx
xor    %eax,%eax
cmp    %rcx,%rsi
seta   %al
movq   $0x0,0xc8(%rsp)
add    %rdx,%rax
mov    $0x10,%r15d
cmp    $0x2,%rax
jb     <inf_store::store::CellStore::new+OFF>
dec    %rax
bsr    %rax,%rcx
not    %ecx
mov    $0xffffffffffffffff,%rax
shr    %cl,%rax
cmp    $0x10,%rax
mov    $0xf,%r15d
cmovae %rax,%r15
inc    %r15
mov    %rbp,0x8(%rsp)
mov    %r15,%rdi
call   *0x0(%rip)        # <malloc@GLIBC_2.2.5>
test   %rax,%rax
je     <inf_store::store::CellStore::new+OFF>
mov    %rax,%r12
mov    %rax,%rdi
mov    $0x80,%esi
mov    %r15,%rdx
call   *0x0(%rip)        # <memset@GLIBC_2.2.5>
lea    0x0(,%r15,8),%r13
mov    %r13,%rdi
call   *0x0(%rip)        # <malloc@GLIBC_2.2.5>
test   %rax,%rax
je     <inf_store::store::CellStore::new+OFF>
mov    %rax,%rbp
add    $0xfffffffffffffff8,%r13
mov    %rax,%rdi
xor    %esi,%esi
mov    %r13,%rdx
call   *0x0(%rip)        # <memset@GLIBC_2.2.5>
movq   $0x0,-0x8(%rbp,%r15,8)
mov    %r12,0x18(%rsp)
mov    %r15,0x20(%rsp)
mov    %rbp,0x28(%rsp)
mov    %r15,0x30(%rsp)
mov    %r15,0x38(%rsp)
xorps  %xmm0,%xmm0
movups %xmm0,0x40(%rsp)
mov    0x8(%rsp),%rax
mov    0x28(%rax),%r13
lea    -0x1(%r13),%rax
mov    %r13,%rcx
xor    %rax,%rcx
cmp    %rax,%rcx
jbe    <inf_store::store::CellStore::new+OFF>
lea    -0x10000(%r13),%rax
cmp    $0x1f0000,%rax
ja     <inf_store::store::CellStore::new+OFF>
shr    $0x2,%r13
mov    $0x100,%eax
mov    $0x1c,%r15d
mov    $0x2e8,%r12d
nopl   0x0(%rax)
add    %rax,%rax
add    $0x4,%r15
add    $0x60,%r12
cmp    %r13,%rax
jb     <inf_store::store::CellStore::new+OFF>
lea    0x3(%r15),%rbp
movabs $0x555555555555555,%rax
cmp    %rax,%rbp
jbe    <inf_store::store::CellStore::new+OFF>
xor    %edi,%edi
mov    %r12,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
jmp    <inf_store::store::CellStore::new+OFF>
mov    %r12,%rdi
call   *0x0(%rip)        # <malloc@GLIBC_2.2.5>
test   %rax,%rax
je     <inf_store::store::CellStore::new+OFF>
test   %r15,%r15
movups 0x0(%rip),%xmm0        # <anon.75fd87380dfc59308f7611a9a6d5ea54.44.llvm.15107721511710138065>
je     <inf_store::store::CellStore::new+OFF>
xor    %edx,%edx
mov    %rax,%rcx
cs nopw 0x0(%rax,%rax,1)
mov    %r14,0x10(%rcx)
movups %xmm0,(%rcx)
movups %xmm0,0x18(%rcx)
mov    %r14,0x28(%rcx)
movups %xmm0,0x30(%rcx)
mov    %r14,0x40(%rcx)
add    $0x4,%rdx
movups %xmm0,0x48(%rcx)
mov    %r14,0x58(%rcx)
add    $0x60,%rcx
cmp    %rdx,%r15
jne    <inf_store::store::CellStore::new+OFF>
jmp    <inf_store::store::CellStore::new+OFF>
xor    %edx,%edx
lea    (%rdx,%rdx,2),%rcx
mov    %rax,%r15
lea    (%rax,%rcx,8),%rax
mov    $0xffffffffffffffb8,%rcx
data16 data16 data16 data16 cs nopw 0x0(%rax,%rax,1)
mov    %r14,0x58(%rax,%rcx,1)
movups %xmm0,0x48(%rax,%rcx,1)
add    $0x18,%rcx
jne    <inf_store::store::CellStore::new+OFF>
mov    0x8(%rsp),%r14
mov    0x28(%r14),%rax
mov    %rax,0x90(%rbx)
movups 0x18(%r14),%xmm0
movups %xmm0,0x80(%rbx)
xorps  %xmm0,%xmm0
movups %xmm0,0xc0(%rbx)
movups %xmm0,0xe8(%rbx)
movq   $0x0,0xf8(%rbx)
movups %xmm0,0x160(%rbx)
movups %xmm0,0x170(%rbx)
movups %xmm0,0x180(%rbx)
movq   $0x0,0x190(%rbx)
movups %xmm0,0x110(%rbx)
movups %xmm0,0x128(%rbx)
movups %xmm0,0x140(%rbx)
lea    0x2320(%rbx),%rdi
mov    $0x400,%edx
xor    %esi,%esi
call   *0x0(%rip)        # <memset@GLIBC_2.2.5>
movaps 0x50(%rsp),%xmm0
movaps 0x60(%rsp),%xmm1
movaps 0x70(%rsp),%xmm2
movaps 0x80(%rsp),%xmm3
movups %xmm0,(%rbx)
movups %xmm1,0x10(%rbx)
movups %xmm2,0x20(%rbx)
movups %xmm3,0x30(%rbx)
movaps 0x90(%rsp),%xmm0
movups %xmm0,0x40(%rbx)
movaps 0xa0(%rsp),%xmm0
movups %xmm0,0x50(%rbx)
movaps 0xb0(%rsp),%xmm0
movups %xmm0,0x60(%rbx)
movaps 0xc0(%rsp),%xmm0
movups %xmm0,0x70(%rbx)
mov    0x48(%rsp),%rax
mov    %rax,0x2788(%rbx)
movups 0x18(%rsp),%xmm0
movups 0x28(%rsp),%xmm1
movups 0x38(%rsp),%xmm2
movups %xmm2,0x2778(%rbx)
movups %xmm1,0x2768(%rbx)
movups %xmm0,0x2758(%rbx)
movq   $0x0,0x210(%rbx)
movq   $0x8,0x218(%rbx)
movq   $0x0,0x220(%rbx)
movaps 0x0(%rip),%xmm0        # <anon.58baeeae7d5fe476157d296c9f08f803.346.llvm.5844774155465566772+OFF>
movups %xmm0,0x228(%rbx)
movups %xmm0,0x238(%rbx)
movups %xmm0,0x248(%rbx)
movups %xmm0,0x258(%rbx)
movups %xmm0,0x268(%rbx)
movups %xmm0,0x278(%rbx)
movups %xmm0,0x288(%rbx)
movups %xmm0,0x298(%rbx)
movups %xmm0,0x2a8(%rbx)
movups %xmm0,0x2b8(%rbx)
movups %xmm0,0x2c8(%rbx)
movups %xmm0,0x2d8(%rbx)
movups %xmm0,0x2e8(%rbx)
movups %xmm0,0x2f8(%rbx)
movups %xmm0,0x308(%rbx)
movups %xmm0,0x318(%rbx)
movups %xmm0,0x328(%rbx)
movups %xmm0,0x338(%rbx)
movups %xmm0,0x348(%rbx)
movups %xmm0,0x358(%rbx)
movups %xmm0,0x368(%rbx)
movups %xmm0,0x378(%rbx)
movups %xmm0,0x388(%rbx)
movups %xmm0,0x398(%rbx)
movups %xmm0,0x3a8(%rbx)
movups %xmm0,0x3b8(%rbx)
movups %xmm0,0x3c8(%rbx)
movups %xmm0,0x3d8(%rbx)
movups %xmm0,0x3e8(%rbx)
movups %xmm0,0x3f8(%rbx)
movups %xmm0,0x408(%rbx)
movups %xmm0,0x418(%rbx)
movups %xmm0,0x428(%rbx)
movups %xmm0,0x438(%rbx)
movups %xmm0,0x448(%rbx)
movups %xmm0,0x458(%rbx)
movups %xmm0,0x468(%rbx)
movups %xmm0,0x478(%rbx)
movups %xmm0,0x488(%rbx)
movups %xmm0,0x498(%rbx)
movups %xmm0,0x4a8(%rbx)
movups %xmm0,0x4b8(%rbx)
movups %xmm0,0x4c8(%rbx)
movups %xmm0,0x4d8(%rbx)
movups %xmm0,0x4e8(%rbx)
movups %xmm0,0x4f8(%rbx)
movups %xmm0,0x508(%rbx)
movups %xmm0,0x518(%rbx)
movups %xmm0,0x528(%rbx)
movups %xmm0,0x538(%rbx)
movups %xmm0,0x548(%rbx)
movups %xmm0,0x558(%rbx)
movups %xmm0,0x568(%rbx)
movups %xmm0,0x578(%rbx)
movups %xmm0,0x588(%rbx)
movups %xmm0,0x598(%rbx)
movups %xmm0,0x5a8(%rbx)
movups %xmm0,0x5b8(%rbx)
movups %xmm0,0x5c8(%rbx)
movups %xmm0,0x5d8(%rbx)
movups %xmm0,0x5e8(%rbx)
movups %xmm0,0x5f8(%rbx)
movups %xmm0,0x608(%rbx)
movups %xmm0,0x618(%rbx)
movups %xmm0,0x628(%rbx)
movups %xmm0,0x638(%rbx)
movups %xmm0,0x648(%rbx)
movups %xmm0,0x658(%rbx)
movups %xmm0,0x668(%rbx)
movups %xmm0,0x678(%rbx)
movups %xmm0,0x688(%rbx)
movups %xmm0,0x698(%rbx)
movups %xmm0,0x6a8(%rbx)
movups %xmm0,0x6b8(%rbx)
movups %xmm0,0x6c8(%rbx)
movups %xmm0,0x6d8(%rbx)
movups %xmm0,0x6e8(%rbx)
movups %xmm0,0x6f8(%rbx)
movups %xmm0,0x708(%rbx)
movups %xmm0,0x718(%rbx)
movups %xmm0,0x728(%rbx)
movups %xmm0,0x738(%rbx)
movups %xmm0,0x748(%rbx)
movups %xmm0,0x758(%rbx)
movups %xmm0,0x768(%rbx)
movups %xmm0,0x778(%rbx)
movups %xmm0,0x788(%rbx)
movups %xmm0,0x798(%rbx)
movups %xmm0,0x7a8(%rbx)
movups %xmm0,0x7b8(%rbx)
movups %xmm0,0x7c8(%rbx)
movups %xmm0,0x7d8(%rbx)
movups %xmm0,0x7e8(%rbx)
movups %xmm0,0x7f8(%rbx)
movups %xmm0,0x808(%rbx)
movups %xmm0,0x818(%rbx)
movups %xmm0,0x828(%rbx)
movups %xmm0,0x838(%rbx)
movups %xmm0,0x848(%rbx)
movups %xmm0,0x858(%rbx)
movups %xmm0,0x868(%rbx)
movups %xmm0,0x878(%rbx)
movups %xmm0,0x888(%rbx)
movups %xmm0,0x898(%rbx)
movups %xmm0,0x8a8(%rbx)
movups %xmm0,0x8b8(%rbx)
movups %xmm0,0x8c8(%rbx)
movups %xmm0,0x8d8(%rbx)
movups %xmm0,0x8e8(%rbx)
movups %xmm0,0x8f8(%rbx)
movups %xmm0,0x908(%rbx)
movups %xmm0,0x918(%rbx)
movups %xmm0,0x928(%rbx)
movups %xmm0,0x938(%rbx)
movups %xmm0,0x948(%rbx)
movups %xmm0,0x958(%rbx)
movups %xmm0,0x968(%rbx)
movups %xmm0,0x978(%rbx)
movups %xmm0,0x988(%rbx)
movups %xmm0,0x998(%rbx)
movups %xmm0,0x9a8(%rbx)
movups %xmm0,0x9b8(%rbx)
movups %xmm0,0x9c8(%rbx)
movups %xmm0,0x9d8(%rbx)
movups %xmm0,0x9e8(%rbx)
movups %xmm0,0x9f8(%rbx)
movups %xmm0,0xa08(%rbx)
movups %xmm0,0xa18(%rbx)
movups %xmm0,0xa28(%rbx)
movups %xmm0,0xa38(%rbx)
movups %xmm0,0xa48(%rbx)
movups %xmm0,0xa58(%rbx)
movups %xmm0,0xa68(%rbx)
movups %xmm0,0xa78(%rbx)
movups %xmm0,0xa88(%rbx)
movups %xmm0,0xa98(%rbx)
movups %xmm0,0xaa8(%rbx)
movups %xmm0,0xab8(%rbx)
movups %xmm0,0xac8(%rbx)
movups %xmm0,0xad8(%rbx)
movups %xmm0,0xae8(%rbx)
movups %xmm0,0xaf8(%rbx)
movups %xmm0,0xb08(%rbx)
movups %xmm0,0xb18(%rbx)
movups %xmm0,0xb28(%rbx)
movups %xmm0,0xb38(%rbx)
movups %xmm0,0xb48(%rbx)
movups %xmm0,0xb58(%rbx)
movups %xmm0,0xb68(%rbx)
movups %xmm0,0xb78(%rbx)
movups %xmm0,0xb88(%rbx)
movups %xmm0,0xb98(%rbx)
movups %xmm0,0xba8(%rbx)
movups %xmm0,0xbb8(%rbx)
movups %xmm0,0xbc8(%rbx)
movups %xmm0,0xbd8(%rbx)
movups %xmm0,0xbe8(%rbx)
movups %xmm0,0xbf8(%rbx)
movups %xmm0,0xc08(%rbx)
movups %xmm0,0xc18(%rbx)
movups %xmm0,0xc28(%rbx)
movups %xmm0,0xc38(%rbx)
movups %xmm0,0xc48(%rbx)
movups %xmm0,0xc58(%rbx)
movups %xmm0,0xc68(%rbx)
movups %xmm0,0xc78(%rbx)
movups %xmm0,0xc88(%rbx)
movups %xmm0,0xc98(%rbx)
movups %xmm0,0xca8(%rbx)
movups %xmm0,0xcb8(%rbx)
movups %xmm0,0xcc8(%rbx)
movups %xmm0,0xcd8(%rbx)
movups %xmm0,0xce8(%rbx)
movups %xmm0,0xcf8(%rbx)
movups %xmm0,0xd08(%rbx)
movups %xmm0,0xd18(%rbx)
movups %xmm0,0xd28(%rbx)
movups %xmm0,0xd38(%rbx)
movups %xmm0,0xd48(%rbx)
movups %xmm0,0xd58(%rbx)
movups %xmm0,0xd68(%rbx)
movups %xmm0,0xd78(%rbx)
movups %xmm0,0xd88(%rbx)
movups %xmm0,0xd98(%rbx)
movups %xmm0,0xda8(%rbx)
movups %xmm0,0xdb8(%rbx)
movups %xmm0,0xdc8(%rbx)
movups %xmm0,0xdd8(%rbx)
movups %xmm0,0xde8(%rbx)
movups %xmm0,0xdf8(%rbx)
movups %xmm0,0xe08(%rbx)
movups %xmm0,0xe18(%rbx)
movups %xmm0,0xe28(%rbx)
movups %xmm0,0xe38(%rbx)
movups %xmm0,0xe48(%rbx)
movups %xmm0,0xe58(%rbx)
movups %xmm0,0xe68(%rbx)
movups %xmm0,0xe78(%rbx)
movups %xmm0,0xe88(%rbx)
movups %xmm0,0xe98(%rbx)
movups %xmm0,0xea8(%rbx)
movups %xmm0,0xeb8(%rbx)
movups %xmm0,0xec8(%rbx)
movups %xmm0,0xed8(%rbx)
movups %xmm0,0xee8(%rbx)
movups %xmm0,0xef8(%rbx)
movups %xmm0,0xf08(%rbx)
movups %xmm0,0xf18(%rbx)
movups %xmm0,0xf28(%rbx)
movups %xmm0,0xf38(%rbx)
movups %xmm0,0xf48(%rbx)
movups %xmm0,0xf58(%rbx)
movups %xmm0,0xf68(%rbx)
movups %xmm0,0xf78(%rbx)
movups %xmm0,0xf88(%rbx)
movups %xmm0,0xf98(%rbx)
movups %xmm0,0xfa8(%rbx)
movups %xmm0,0xfb8(%rbx)
movups %xmm0,0xfc8(%rbx)
movups %xmm0,0xfd8(%rbx)
movups %xmm0,0xfe8(%rbx)
movups %xmm0,0xff8(%rbx)
movups %xmm0,0x1008(%rbx)
movups %xmm0,0x1018(%rbx)
movups %xmm0,0x1028(%rbx)
movups %xmm0,0x1038(%rbx)
movups %xmm0,0x1048(%rbx)
movups %xmm0,0x1058(%rbx)
movups %xmm0,0x1068(%rbx)
movups %xmm0,0x1078(%rbx)
movups %xmm0,0x1088(%rbx)
movups %xmm0,0x1098(%rbx)
movups %xmm0,0x10a8(%rbx)
movups %xmm0,0x10b8(%rbx)
movups %xmm0,0x10c8(%rbx)
movups %xmm0,0x10d8(%rbx)
movups %xmm0,0x10e8(%rbx)
movups %xmm0,0x10f8(%rbx)
movups %xmm0,0x1108(%rbx)
movups %xmm0,0x1118(%rbx)
movups %xmm0,0x1128(%rbx)
movups %xmm0,0x1138(%rbx)
movups %xmm0,0x1148(%rbx)
movups %xmm0,0x1158(%rbx)
movups %xmm0,0x1168(%rbx)
movups %xmm0,0x1178(%rbx)
movups %xmm0,0x1188(%rbx)
movups %xmm0,0x1198(%rbx)
movups %xmm0,0x11a8(%rbx)
movups %xmm0,0x11b8(%rbx)
movups %xmm0,0x11c8(%rbx)
movups %xmm0,0x11d8(%rbx)
movups %xmm0,0x11e8(%rbx)
movups %xmm0,0x11f8(%rbx)
movups %xmm0,0x1208(%rbx)
movups %xmm0,0x1218(%rbx)
movups %xmm0,0x1228(%rbx)
movups %xmm0,0x1238(%rbx)
movups %xmm0,0x1248(%rbx)
movups %xmm0,0x1258(%rbx)
movups %xmm0,0x1268(%rbx)
movups %xmm0,0x1278(%rbx)
movups %xmm0,0x1288(%rbx)
movups %xmm0,0x1298(%rbx)
movups %xmm0,0x12a8(%rbx)
movups %xmm0,0x12b8(%rbx)
movups %xmm0,0x12c8(%rbx)
movups %xmm0,0x12d8(%rbx)
movups %xmm0,0x12e8(%rbx)
movups %xmm0,0x12f8(%rbx)
movups %xmm0,0x1308(%rbx)
movups %xmm0,0x1318(%rbx)
movups %xmm0,0x1328(%rbx)
movups %xmm0,0x1338(%rbx)
movups %xmm0,0x1348(%rbx)
movups %xmm0,0x1358(%rbx)
movups %xmm0,0x1368(%rbx)
movups %xmm0,0x1378(%rbx)
movups %xmm0,0x1388(%rbx)
movups %xmm0,0x1398(%rbx)
movups %xmm0,0x13a8(%rbx)
movups %xmm0,0x13b8(%rbx)
movups %xmm0,0x13c8(%rbx)
movups %xmm0,0x13d8(%rbx)
movups %xmm0,0x13e8(%rbx)
movups %xmm0,0x13f8(%rbx)
movups %xmm0,0x1408(%rbx)
movups %xmm0,0x1418(%rbx)
movups %xmm0,0x1428(%rbx)
movups %xmm0,0x1438(%rbx)
movups %xmm0,0x1448(%rbx)
movups %xmm0,0x1458(%rbx)
movups %xmm0,0x1468(%rbx)
movups %xmm0,0x1478(%rbx)
movups %xmm0,0x1488(%rbx)
movups %xmm0,0x1498(%rbx)
movups %xmm0,0x14a8(%rbx)
movups %xmm0,0x14b8(%rbx)
movups %xmm0,0x14c8(%rbx)
movups %xmm0,0x14d8(%rbx)
movups %xmm0,0x14e8(%rbx)
movups %xmm0,0x14f8(%rbx)
movups %xmm0,0x1508(%rbx)
movups %xmm0,0x1518(%rbx)
movups %xmm0,0x1528(%rbx)
movups %xmm0,0x1538(%rbx)
movups %xmm0,0x1548(%rbx)
movups %xmm0,0x1558(%rbx)
movups %xmm0,0x1568(%rbx)
movups %xmm0,0x1578(%rbx)
movups %xmm0,0x1588(%rbx)
movups %xmm0,0x1598(%rbx)
movups %xmm0,0x15a8(%rbx)
movups %xmm0,0x15b8(%rbx)
movups %xmm0,0x15c8(%rbx)
movups %xmm0,0x15d8(%rbx)
movups %xmm0,0x15e8(%rbx)
movups %xmm0,0x15f8(%rbx)
movups %xmm0,0x1608(%rbx)
movups %xmm0,0x1618(%rbx)
movups %xmm0,0x1628(%rbx)
movups %xmm0,0x1638(%rbx)
movups %xmm0,0x1648(%rbx)
movups %xmm0,0x1658(%rbx)
movups %xmm0,0x1668(%rbx)
movups %xmm0,0x1678(%rbx)
movups %xmm0,0x1688(%rbx)
movups %xmm0,0x1698(%rbx)
movups %xmm0,0x16a8(%rbx)
movups %xmm0,0x16b8(%rbx)
movups %xmm0,0x16c8(%rbx)
movups %xmm0,0x16d8(%rbx)
movups %xmm0,0x16e8(%rbx)
movups %xmm0,0x16f8(%rbx)
movups %xmm0,0x1708(%rbx)
movups %xmm0,0x1718(%rbx)
movups %xmm0,0x1728(%rbx)
movups %xmm0,0x1738(%rbx)
movups %xmm0,0x1748(%rbx)
movups %xmm0,0x1758(%rbx)
movups %xmm0,0x1768(%rbx)
movups %xmm0,0x1778(%rbx)
movups %xmm0,0x1788(%rbx)
movups %xmm0,0x1798(%rbx)
movups %xmm0,0x17a8(%rbx)
movups %xmm0,0x17b8(%rbx)
movups %xmm0,0x17c8(%rbx)
movups %xmm0,0x17d8(%rbx)
movups %xmm0,0x17e8(%rbx)
movups %xmm0,0x17f8(%rbx)
movups %xmm0,0x1808(%rbx)
movups %xmm0,0x1818(%rbx)
movups %xmm0,0x1828(%rbx)
movups %xmm0,0x1838(%rbx)
movups %xmm0,0x1848(%rbx)
movups %xmm0,0x1858(%rbx)
movups %xmm0,0x1868(%rbx)
movups %xmm0,0x1878(%rbx)
movups %xmm0,0x1888(%rbx)
movups %xmm0,0x1898(%rbx)
movups %xmm0,0x18a8(%rbx)
movups %xmm0,0x18b8(%rbx)
movups %xmm0,0x18c8(%rbx)
movups %xmm0,0x18d8(%rbx)
movups %xmm0,0x18e8(%rbx)
movups %xmm0,0x18f8(%rbx)
movups %xmm0,0x1908(%rbx)
movups %xmm0,0x1918(%rbx)
movups %xmm0,0x1928(%rbx)
movups %xmm0,0x1938(%rbx)
movups %xmm0,0x1948(%rbx)
movups %xmm0,0x1958(%rbx)
movups %xmm0,0x1968(%rbx)
movups %xmm0,0x1978(%rbx)
movups %xmm0,0x1988(%rbx)
movups %xmm0,0x1998(%rbx)
movups %xmm0,0x19a8(%rbx)
movups %xmm0,0x19b8(%rbx)
movups %xmm0,0x19c8(%rbx)
movups %xmm0,0x19d8(%rbx)
movups %xmm0,0x19e8(%rbx)
movups %xmm0,0x19f8(%rbx)
movups %xmm0,0x1a08(%rbx)
movups %xmm0,0x1a18(%rbx)
movups %xmm0,0x1a28(%rbx)
movups %xmm0,0x1a38(%rbx)
movups %xmm0,0x1a48(%rbx)
movups %xmm0,0x1a58(%rbx)
movups %xmm0,0x1a68(%rbx)
movups %xmm0,0x1a78(%rbx)
movups %xmm0,0x1a88(%rbx)
movups %xmm0,0x1a98(%rbx)
movups %xmm0,0x1aa8(%rbx)
movups %xmm0,0x1ab8(%rbx)
movups %xmm0,0x1ac8(%rbx)
movups %xmm0,0x1ad8(%rbx)
movups %xmm0,0x1ae8(%rbx)
movups %xmm0,0x1af8(%rbx)
movups %xmm0,0x1b08(%rbx)
movups %xmm0,0x1b18(%rbx)
movups %xmm0,0x1b28(%rbx)
movups %xmm0,0x1b38(%rbx)
movups %xmm0,0x1b48(%rbx)
movups %xmm0,0x1b58(%rbx)
movups %xmm0,0x1b68(%rbx)
movups %xmm0,0x1b78(%rbx)
movups %xmm0,0x1b88(%rbx)
movups %xmm0,0x1b98(%rbx)
movups %xmm0,0x1ba8(%rbx)
movups %xmm0,0x1bb8(%rbx)
movups %xmm0,0x1bc8(%rbx)
movups %xmm0,0x1bd8(%rbx)
movups %xmm0,0x1be8(%rbx)
movups %xmm0,0x1bf8(%rbx)
movups %xmm0,0x1c08(%rbx)
movups %xmm0,0x1c18(%rbx)
movups %xmm0,0x1c28(%rbx)
movups %xmm0,0x1c38(%rbx)
movups %xmm0,0x1c48(%rbx)
movups %xmm0,0x1c58(%rbx)
movups %xmm0,0x1c68(%rbx)
movups %xmm0,0x1c78(%rbx)
movups %xmm0,0x1c88(%rbx)
movups %xmm0,0x1c98(%rbx)
movups %xmm0,0x1ca8(%rbx)
movups %xmm0,0x1cb8(%rbx)
movups %xmm0,0x1cc8(%rbx)
movups %xmm0,0x1cd8(%rbx)
movups %xmm0,0x1ce8(%rbx)
movups %xmm0,0x1cf8(%rbx)
movups %xmm0,0x1d08(%rbx)
movups %xmm0,0x1d18(%rbx)
movups %xmm0,0x1d28(%rbx)
movups %xmm0,0x1d38(%rbx)
movups %xmm0,0x1d48(%rbx)
movups %xmm0,0x1d58(%rbx)
movups %xmm0,0x1d68(%rbx)
movups %xmm0,0x1d78(%rbx)
movups %xmm0,0x1d88(%rbx)
movups %xmm0,0x1d98(%rbx)
movups %xmm0,0x1da8(%rbx)
movups %xmm0,0x1db8(%rbx)
movups %xmm0,0x1dc8(%rbx)
movups %xmm0,0x1dd8(%rbx)
movups %xmm0,0x1de8(%rbx)
movups %xmm0,0x1df8(%rbx)
movups %xmm0,0x1e08(%rbx)
movups %xmm0,0x1e18(%rbx)
movups %xmm0,0x1e28(%rbx)
movups %xmm0,0x1e38(%rbx)
movups %xmm0,0x1e48(%rbx)
movups %xmm0,0x1e58(%rbx)
movups %xmm0,0x1e68(%rbx)
movups %xmm0,0x1e78(%rbx)
movups %xmm0,0x1e88(%rbx)
movups %xmm0,0x1e98(%rbx)
movups %xmm0,0x1ea8(%rbx)
movups %xmm0,0x1eb8(%rbx)
movups %xmm0,0x1ec8(%rbx)
movups %xmm0,0x1ed8(%rbx)
movups %xmm0,0x1ee8(%rbx)
movups %xmm0,0x1ef8(%rbx)
movups %xmm0,0x1f08(%rbx)
movups %xmm0,0x1f18(%rbx)
movups %xmm0,0x1f28(%rbx)
movups %xmm0,0x1f38(%rbx)
movups %xmm0,0x1f48(%rbx)
movups %xmm0,0x1f58(%rbx)
movups %xmm0,0x1f68(%rbx)
movups %xmm0,0x1f78(%rbx)
movups %xmm0,0x1f88(%rbx)
movups %xmm0,0x1f98(%rbx)
movups %xmm0,0x1fa8(%rbx)
movups %xmm0,0x1fb8(%rbx)
movups %xmm0,0x1fc8(%rbx)
movups %xmm0,0x1fd8(%rbx)
movups %xmm0,0x1fe8(%rbx)
movups %xmm0,0x1ff8(%rbx)
movups %xmm0,0x2008(%rbx)
movups %xmm0,0x2018(%rbx)
movups %xmm0,0x2028(%rbx)
movups %xmm0,0x2038(%rbx)
movups %xmm0,0x2048(%rbx)
movups %xmm0,0x2058(%rbx)
movups %xmm0,0x2068(%rbx)
movups %xmm0,0x2078(%rbx)
movups %xmm0,0x2088(%rbx)
movups %xmm0,0x2098(%rbx)
movups %xmm0,0x20a8(%rbx)
movups %xmm0,0x20b8(%rbx)
movups %xmm0,0x20c8(%rbx)
movups %xmm0,0x20d8(%rbx)
movups %xmm0,0x20e8(%rbx)
movups %xmm0,0x20f8(%rbx)
movups %xmm0,0x2108(%rbx)
movups %xmm0,0x2118(%rbx)
movups %xmm0,0x2128(%rbx)
movups %xmm0,0x2138(%rbx)
movups %xmm0,0x2148(%rbx)
movups %xmm0,0x2158(%rbx)
movups %xmm0,0x2168(%rbx)
movups %xmm0,0x2178(%rbx)
movups %xmm0,0x2188(%rbx)
movups %xmm0,0x2198(%rbx)
movups %xmm0,0x21a8(%rbx)
movups %xmm0,0x21b8(%rbx)
movups %xmm0,0x21c8(%rbx)
movups %xmm0,0x21d8(%rbx)
movups %xmm0,0x21e8(%rbx)
movups %xmm0,0x21f8(%rbx)
movups %xmm0,0x2208(%rbx)
movups %xmm0,0x2218(%rbx)
xorps  %xmm0,%xmm0
movups %xmm0,0x2248(%rbx)
movups %xmm0,0x2238(%rbx)
movups %xmm0,0x2228(%rbx)
movq   $0x0,0x2258(%rbx)
movabs $0xffffff00ffffff,%rax
mov    %rax,0x2260(%rbx)
movups %xmm0,0x2730(%rbx)
movups %xmm0,0x27c0(%rbx)
movups %xmm0,0x27b0(%rbx)
movups %xmm0,0x27a0(%rbx)
movups %xmm0,0x2790(%rbx)
mov    0x10(%rsp),%rax
mov    %rax,0x2740(%rbx)
movq   $0x0,0x2748(%rbx)
movw   $0x0,0x2750(%rbx)
mov    %rbp,0x98(%rbx)
mov    %r15,0xa0(%rbx)
mov    %rbp,0xa8(%rbx)
movq   $0x0,0xb0(%rbx)
movq   $0x8,0xb8(%rbx)
movq   $0x4,0xd0(%rbx)
movq   $0x0,0xd8(%rbx)
mov    %r13,0xe0(%rbx)
movq   $0x0,0x100(%rbx)
movq   $0x1,0x108(%rbx)
movq   $0x1,0x120(%rbx)
movq   $0x8,0x138(%rbx)
movq   $0x8,0x150(%rbx)
movq   $0x0,0x158(%rbx)
movq   $0x1,0x198(%rbx)
movq   $0x0,0x2268(%rbx)
movq   $0x8,0x2270(%rbx)
movups %xmm0,0x2278(%rbx)
movq   $0x1,0x2288(%rbx)
movups %xmm0,0x2290(%rbx)
movq   $0x8,0x22a0(%rbx)
movups %xmm0,0x22a8(%rbx)
movq   $0x8,0x22b8(%rbx)
movups %xmm0,0x22c0(%rbx)
movq   $0x8,0x22d0(%rbx)
movups %xmm0,0x22d8(%rbx)
movq   $0x1,0x22e8(%rbx)
movups %xmm0,0x22f0(%rbx)
movq   $0x8,0x2300(%rbx)
movups %xmm0,0x2308(%rbx)
movq   $0x0,0x2318(%rbx)
movw   $0x0,0x2720(%rbx)
movb   $0x0,0x2722(%rbx)
movw   $0x200,0x2728(%rbx)
movups 0x60(%r14),%xmm0
movups %xmm0,0x200(%rbx)
movups 0x50(%r14),%xmm0
movups %xmm0,0x1f0(%rbx)
movups 0x40(%r14),%xmm0
movups %xmm0,0x1e0(%rbx)
movups (%r14),%xmm0
movups 0x10(%r14),%xmm1
movups 0x20(%r14),%xmm2
movups 0x30(%r14),%xmm3
movups %xmm3,0x1d0(%rbx)
movups %xmm2,0x1c0(%rbx)
movups %xmm1,0x1b0(%rbx)
movups %xmm0,0x1a0(%rbx)
mov    %rbx,%rax
add    $0xd8,%rsp
pop    %rbx
pop    %r12
pop    %r13
pop    %r14
pop    %r15
pop    %rbp
ret
lea    0x0(%rip),%rdi        # <anon.75fd87380dfc59308f7611a9a6d5ea54.42.llvm.15107721511710138065>
lea    0x0(%rip),%rdx        # <anon.75fd87380dfc59308f7611a9a6d5ea54.43.llvm.15107721511710138065>
mov    $0x69,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdi        # <anon.75fd87380dfc59308f7611a9a6d5ea54.42.llvm.15107721511710138065>
lea    0x0(%rip),%rdx        # <anon.75fd87380dfc59308f7611a9a6d5ea54.43.llvm.15107721511710138065>
mov    $0x69,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
jmp    <inf_store::store::CellStore::new+OFF>
mov    $0x8,%edi
mov    %r15,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
mov    $0x1,%edi
mov    %r15,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
jmp    <inf_store::store::CellStore::new+OFF>
mov    $0x8,%edi
mov    %r13,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
ud2
mov    $0x8,%edi
jmp    <inf_store::store::CellStore::new+OFF>
mov    %rax,%rbx
mov    %r12,%rdi
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
lea    0x50(%rsp),%rdi
call   <core::ptr::drop_in_place<inf_alloc::arena::Arena>>
mov    %rbx,%rdi
call   <_Unwind_Resume@plt>
mov    %rax,%rbx
lea    0x50(%rsp),%rdi
call   <core::ptr::drop_in_place<inf_alloc::arena::Arena>>
mov    %rbx,%rdi
call   <_Unwind_Resume@plt>
mov    %rax,%rbx
lea    0x18(%rsp),%rdi
call   <core::ptr::drop_in_place<inf_store::index::Index>>
lea    0x50(%rsp),%rdi
call   <core::ptr::drop_in_place<inf_alloc::arena::Arena>>
mov    %rbx,%rdi
call   <_Unwind_Resume@plt>
int3
int3

<inf_store::store::CellStore::set>:
push   %rbp
push   %r15
push   %r14
push   %r13
push   %r12
push   %rbx
sub    $0xf8,%rsp
mov    %rcx,%r13
mov    %rdi,%rbx
cmp    $0x100,%rcx
setae  %al
cmp    $0x1000000,%r9
setae  %cl
or     %al,%cl
je     <inf_store::store::CellStore::set+OFF>
movq   $0x7,0x8(%rbx)
movq   $0x2,(%rbx)
jmp    <inf_store::store::CellStore::set+OFF>
mov    %r9,%r12
mov    %rdx,%rbp
mov    %rsi,%r14
mov    %r8,0x70(%rsp)
mov    0x138(%rsp),%r15
movabs $0x8000000000000000,%rax
mov    %rax,0x8(%rsp)
movabs $0x1af1d8a50db5eed1,%rdx
mov    %rbp,%rdi
mov    %r13,%rsi
call   <inf_foundation::hash::hash64>
lea    0x30(%rsp),%rdi
mov    %r14,%rsi
mov    %rbp,%rdx
mov    %r13,%rcx
mov    %rax,%r8
mov    %r15,%r9
call   *0x0(%rip)        # <_DYNAMIC+OFF>
mov    0x30(%rsp),%r8
cmp    $0x1,%r8
jne    <inf_store::store::CellStore::set+OFF>
mov    0x38(%rsp),%r15
mov    0x40(%r14),%rsi
mov    %r15,%rdi
shr    $0x15,%rdi
cmp    %rsi,%rdi
jae    <inf_store::store::CellStore::set+OFF>
mov    0x40(%rsp),%rdx
mov    0x38(%r14),%rax
shl    $0x4,%rdi
and    $0x1fffff,%r15d
lea    (%r15,%rdx,1),%rcx
cmp    0x8(%rax,%rdi,1),%rcx
ja     <inf_store::store::CellStore::set+OFF>
add    %rdi,%rax
mov    (%rax),%rax
add    %rax,%r15
mov    0x130(%rsp),%r9
cmpb   $0x0,0x10(%r9)
je     <inf_store::store::CellStore::set+OFF>
test   %rax,%rax
je     <inf_store::store::CellStore::set+OFF>
test   %rdx,%rdx
je     <inf_store::store::CellStore::set+OFF>
movzbl (%r15),%eax
mov    %eax,%ecx
shr    $0x4,%cl
lea    -0x1(%rcx),%esi
cmp    $0x3,%sil
jae    <inf_store::store::CellStore::set+OFF>
cmp    $0x1,%cl
jne    <inf_store::store::CellStore::set+OFF>
test   $0x1,%al
mov    %rbp,0x28(%rsp)
jne    <inf_store::store::CellStore::set+OFF>
cmp    $0x1,%rdx
je     <inf_store::store::CellStore::set+OFF>
cmp    $0x4,%rdx
jbe    <inf_store::store::CellStore::set+OFF>
movzbl 0x1(%r15),%edi
add    $0x8,%rdi
movq   $0x0,0x10(%rsp)
jmp    <inf_store::store::CellStore::set+OFF>
xor    %r15d,%r15d
movq   $0x0,0x10(%rsp)
mov    0x130(%rsp),%r9
jmp    <inf_store::store::CellStore::set+OFF>
test   %rax,%rax
je     <inf_store::store::CellStore::set+OFF>
test   %rdx,%rdx
je     <inf_store::store::CellStore::set+OFF>
testb  $0x1,(%r15)
jne    <inf_store::store::CellStore::set+OFF>
movq   $0x0,0x10(%rsp)
jmp    <inf_store::store::CellStore::set+OFF>
xor    %r15d,%r15d
movq   $0x0,0x10(%rsp)
jmp    <inf_store::store::CellStore::set+OFF>
cmp    $0xc,%rdx
jbe    <inf_store::store::CellStore::set+OFF>
mov    0x8(%r15),%eax
movzbl 0xc(%r15),%ecx
shl    $0x20,%rcx
or     %rax,%rcx
mov    %rcx,0x20(%rsp)
mov    $0x1,%eax
mov    %rax,0x10(%rsp)
jmp    <inf_store::store::CellStore::set+OFF>
movq   $0x8,0x8(%rbx)
jmp    <inf_store::store::CellStore::set+OFF>
cmp    $0xc,%rdx
jbe    <inf_store::store::CellStore::set+OFF>
mov    0x8(%r15),%eax
movzbl 0xc(%r15),%ecx
shl    $0x20,%rcx
or     %rax,%rcx
mov    %rcx,0x20(%rsp)
movzbl 0x1(%r15),%edi
add    $0xd,%rdi
mov    $0x1,%eax
mov    %rax,0x10(%rsp)
mov    %r8,(%rsp)
movzwl 0x2(%r15),%eax
movzbl 0x4(%r15),%ebp
shl    $0x10,%ebp
or     %rax,%rbp
lea    (%rdi,%rbp,1),%rsi
cmp    %rdx,%rsi
ja     <inf_store::store::CellStore::set+OFF>
test   %rbp,%rbp
je     <inf_store::store::CellStore::set+OFF>
mov    %rdi,0x8(%rsp)
mov    %rdx,0x68(%rsp)
mov    %rbp,%rdi
call   *0x0(%rip)        # <malloc@GLIBC_2.2.5>
test   %rax,%rax
je     <inf_store::store::CellStore::set+OFF>
mov    0x8(%rsp),%rsi
add    %r15,%rsi
mov    %rax,0x18(%rsp)
mov    %rax,%rdi
mov    %rbp,0x8(%rsp)
mov    %rbp,%rdx
call   *0x0(%rip)        # <memcpy@GLIBC_2.14>
mov    0x28(%rsp),%rbp
mov    (%rsp),%r8
mov    0x130(%rsp),%r9
mov    0x68(%rsp),%rdx
jmp    <inf_store::store::CellStore::set+OFF>
mov    $0x1,%eax
mov    %rax,0x18(%rsp)
movq   $0x0,0x8(%rsp)
mov    0x28(%rsp),%rbp
mov    (%rsp),%r8
movzbl 0x11(%r9),%eax
test   %eax,%eax
je     <inf_store::store::CellStore::set+OFF>
cmp    $0x1,%eax
jne    <inf_store::store::CellStore::set+OFF>
test   %r8b,%r8b
je     <inf_store::store::CellStore::set+OFF>
movq   $0x1,(%rbx)
mov    0x8(%rsp),%rax
mov    %rax,0x8(%rbx)
mov    0x18(%rsp),%rcx
mov    %rcx,0x10(%rbx)
mov    %rax,0x18(%rbx)
jmp    <inf_store::store::CellStore::set+OFF>
test   %r8b,%r8b
je     <inf_store::store::CellStore::set+OFF>
test   %r15,%r15
mov    %r14,(%rsp)
je     <inf_store::store::CellStore::set+OFF>
cmp    $0x7,%rdx
jbe    <inf_store::store::CellStore::set+OFF>
movzbl 0x7(%r15),%eax
shl    $0x10,%eax
movzwl 0x5(%r15),%ecx
add    %eax,%ecx
inc    %ecx
mov    (%r9),%r14
test   %r14,%r14
je     <inf_store::store::CellStore::set+OFF>
mov    0x20(%rsp),%r15
cmp    $0x2,%r14d
mov    0x10(%rsp),%r14
jne    <inf_store::store::CellStore::set+OFF>
movabs $0x431bde82d7b634db,%rax
mulq   0x8(%r9)
mov    %rdx,%r15
shr    $0x12,%r15
movabs $0xffffffffff,%rax
cmp    %rax,%r15
cmovae %rax,%r15
mov    $0x1,%r14d
jmp    <inf_store::store::CellStore::set+OFF>
mov    $0x1,%ecx
mov    (%r9),%r14
test   %r14,%r14
jne    <inf_store::store::CellStore::set+OFF>
mov    %rbp,0x88(%rsp)
mov    %r13,0x90(%rsp)
mov    0x70(%rsp),%rax
mov    %rax,0x98(%rsp)
mov    %r12,0xa0(%rsp)
mov    %ecx,0xa8(%rsp)
mov    %r14,0x78(%rsp)
mov    %r15,0x80(%rsp)
movb   $0x0,0xac(%rsp)
test   %r8b,%r8b
je     <inf_store::store::CellStore::set+OFF>
mov    0x38(%rsp),%rcx
mov    0x40(%rsp),%r8
mov    (%rsp),%rax
mov    0x38(%rax),%rsi
mov    0x40(%rax),%rdx
lea    0x48(%rsp),%rdi
call   <inf_store::doc::payload_of>
mov    0x48(%rsp),%r12d
movups 0x4c(%rsp),%xmm0
movaps %xmm0,0xb0(%rsp)
movups 0x58(%rsp),%xmm0
movups %xmm0,0xbc(%rsp)
lea    0x48(%rsp),%rdi
lea    0x30(%rsp),%r8
lea    0x78(%rsp),%r9
mov    (%rsp),%rsi
mov    %rbp,%rdx
mov    %r13,%rcx
call   <inf_store::store::CellStore::write_record_carrying>
mov    0x48(%rsp),%rax
cmp    $0xb,%rax
jne    <inf_store::store::CellStore::set+OFF>
mov    %r12d,0xd8(%rsp)
movaps 0xb0(%rsp),%xmm0
movups %xmm0,0xdc(%rsp)
movups 0xbc(%rsp),%xmm0
movups %xmm0,0xe8(%rsp)
mov    (%rsp),%rax
lea    0x80(%rax),%rdi
lea    0xd8(%rsp),%rsi
call   <inf_store::doc::DocStore::release>
jmp    <inf_store::store::CellStore::set+OFF>
lea    0x48(%rsp),%rdi
lea    0x30(%rsp),%r8
lea    0x78(%rsp),%r9
mov    (%rsp),%rsi
mov    %rbp,%rdx
mov    %r13,%rcx
call   <inf_store::store::CellStore::write_record_carrying>
mov    0x48(%rsp),%rax
cmp    $0xb,%rax
jne    <inf_store::store::CellStore::set+OFF>
cmpq   $0x0,0x10(%rsp)
je     <inf_store::store::CellStore::set+OFF>
cmp    $0x1,%r14d
mov    0x8(%rsp),%rdx
jne    <inf_store::store::CellStore::set+OFF>
cmp    %r15,0x20(%rsp)
jne    <inf_store::store::CellStore::set+OFF>
jmp    <inf_store::store::CellStore::set+OFF>
movups 0x50(%rsp),%xmm0
movups %xmm0,0x10(%rbx)
mov    %rax,0x8(%rbx)
movq   $0x2,(%rbx)
mov    0x8(%rsp),%rax
shl    $1,%rax
test   %rax,%rax
je     <inf_store::store::CellStore::set+OFF>
mov    0x18(%rsp),%rdi
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
jmp    <inf_store::store::CellStore::set+OFF>
cmp    $0x1,%r14d
mov    0x8(%rsp),%rdx
jne    <inf_store::store::CellStore::set+OFF>
mov    (%rsp),%rax
incq   0x27b0(%rax)
movabs $0x1af1d8a50db5eed1,%rdx
mov    %rbp,%rdi
mov    %r13,%rsi
call   <inf_foundation::hash::hash64>
mov    %rax,%r13
movabs $0xffffffffff,%rbp
cmp    %rbp,%r15
cmovae %rbp,%r15
mov    (%rsp),%r10
mov    0x2260(%r10),%edi
cmp    $0xffffff,%rdi
jne    <inf_store::store::CellStore::set+OFF>
mov    0x220(%r10),%r12
mov    $0x27c0,%eax
cmp    $0xfffffe,%r12
ja     <inf_store::store::CellStore::set+OFF>
lea    0x210(%r10),%rdi
cmp    (%rdi),%r12
jne    <inf_store::store::CellStore::set+OFF>
call   *0x0(%rip)        # <_DYNAMIC+OFF>
movabs $0xffffff0000000000,%rax
or     %rax,%r15
mov    (%rsp),%r10
mov    0x218(%r10),%rax
mov    %r12,%rcx
shl    $0x4,%rcx
mov    %r13,(%rax,%rcx,1)
mov    %r15,0x8(%rax,%rcx,1)
inc    %r12
mov    %r12,0x220(%r10)
lea    -0x1(%r12),%edi
jmp    <inf_store::store::CellStore::set+OFF>
mov    (%rsp),%rcx
mov    0x27b0(%rcx),%rax
cmp    $0x1,%rax
adc    $0xffffffffffffffff,%rax
mov    %rax,0x27b0(%rcx)
jmp    <inf_store::store::CellStore::set+OFF>
mov    0x220(%r10),%r12
cmp    %rdi,%r12
jbe    <inf_store::store::CellStore::set+OFF>
mov    0x218(%r10),%rax
mov    %rdi,%rcx
shl    $0x4,%rcx
mov    0x8(%rax,%rcx,1),%rdx
shr    $0x28,%rdx
mov    %edx,0x2260(%r10)
movabs $0xffffff0000000000,%rdx
or     %rdx,%r15
mov    %r13,(%rax,%rcx,1)
mov    %r15,0x8(%rax,%rcx,1)
mov    %edi,%edx
cmp    %rdi,%r12
jbe    <inf_store::store::CellStore::set+OFF>
shl    $0x4,%rdi
mov    0x8(%rax,%rdi,1),%rsi
and    %rbp,%rsi
xor    %ecx,%ecx
mov    %rsi,%rdi
sub    0x2250(%r10),%rdi
cmovae %rdi,%rcx
mov    %rcx,%rdi
shr    $0x24,%rdi
je     <inf_store::store::CellStore::set+OFF>
mov    %edx,%edi
cmp    %rdi,%r12
jbe    <inf_store::store::CellStore::set+OFF>
shl    $0x4,%rdi
and    0x8(%rax,%rdi,1),%rbp
mov    0x2264(%r10),%ecx
shl    $0x28,%rcx
or     %rbp,%rcx
mov    %rcx,0x8(%rax,%rdi,1)
mov    %edx,0x2264(%r10)
incq   0x2248(%r10)
jmp    <inf_store::store::CellStore::set+OFF>
cmp    $0x200,%rcx
jae    <inf_store::store::CellStore::set+OFF>
xor    %r8d,%r8d
jmp    <inf_store::store::CellStore::set+OFF>
mov    $0x1,%r8d
cmp    $0x40000,%rcx
jb     <inf_store::store::CellStore::set+OFF>
cmp    $0x8000000,%rcx
mov    $0x3,%r8d
sbb    $0x0,%r8
mov    %edx,%edi
cmp    %rdi,%r12
jbe    <inf_store::store::CellStore::set+OFF>
lea    (%r8,%r8,8),%ecx
shr    %cl,%rsi
and    $0x1ff,%esi
shl    $0x4,%rdi
mov    %r8,%rcx
shl    $0xb,%rcx
add    %r10,%rcx
and    0x8(%rax,%rdi,1),%rbp
mov    0x228(%rcx,%rsi,4),%r9d
shl    $0x28,%r9
or     %rbp,%r9
mov    %r9,0x8(%rax,%rdi,1)
mov    %edx,0x228(%rcx,%rsi,4)
incq   0x2228(%r10,%r8,8)
mov    $0x2258,%eax
incq   (%r10,%rax,1)
mov    0x8(%rsp),%rdx
movq   $0x0,(%rbx)
mov    %rdx,0x8(%rbx)
mov    0x18(%rsp),%rax
mov    %rax,0x10(%rbx)
mov    %rdx,0x18(%rbx)
mov    %rbx,%rax
add    $0xf8,%rsp
pop    %rbx
pop    %r12
pop    %r13
pop    %r14
pop    %r15
pop    %rbp
ret
lea    0x0(%rip),%rdi        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.322.llvm.11790107869987619155+OFF>
lea    0x0(%rip),%rdx        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.323.llvm.11790107869987619155+OFF>
mov    $0x63,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rcx        # <anon.c3ede78c9b5129e1230e526cdf53562a.408.llvm.7438497109383721384+OFF>
mov    $0x5,%edi
mov    $0x8,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
jmp    <inf_store::store::CellStore::set+OFF>
lea    0x0(%rip),%rcx        # <anon.c3ede78c9b5129e1230e526cdf53562a.408.llvm.7438497109383721384+OFF>
mov    $0x8,%edi
mov    $0xd,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rcx        # <anon.c3ede78c9b5129e1230e526cdf53562a.408.llvm.7438497109383721384+OFF>
mov    $0x2,%edi
mov    $0x5,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rcx        # <anon.c3ede78c9b5129e1230e526cdf53562a.408.llvm.7438497109383721384+OFF>
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdi        # <anon.58baeeae7d5fe476157d296c9f08f803.1.llvm.5844774155465566772+OFF>
lea    0x0(%rip),%rdx        # <anon.c3ede78c9b5129e1230e526cdf53562a.408.llvm.7438497109383721384+OFF>
mov    $0x10,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.323.llvm.11790107869987619155+OFF>
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.c3ede78c9b5129e1230e526cdf53562a.408.llvm.7438497109383721384+OFF>
xor    %edi,%edi
xor    %esi,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.c3ede78c9b5129e1230e526cdf53562a.408.llvm.7438497109383721384+OFF>
xor    %edi,%edi
xor    %esi,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.c3ede78c9b5129e1230e526cdf53562a.408.llvm.7438497109383721384+OFF>
jmp    <inf_store::store::CellStore::set+OFF>
lea    0x0(%rip),%rdx        # <anon.c3ede78c9b5129e1230e526cdf53562a.408.llvm.7438497109383721384+OFF>
jmp    <inf_store::store::CellStore::set+OFF>
mov    $0x1,%edi
mov    %rbp,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.c3ede78c9b5129e1230e526cdf53562a.408.llvm.7438497109383721384+OFF>
jmp    <inf_store::store::CellStore::set+OFF>
lea    0x0(%rip),%rdx        # <anon.c3ede78c9b5129e1230e526cdf53562a.408.llvm.7438497109383721384+OFF>
mov    %r12,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
ud2
lea    0x0(%rip),%rdx        # <anon.4538fc0f0f6c4a4dec4317bb113f19f8.323.llvm.11790107869987619155+OFF>
mov    $0x1,%edi
mov    $0x1,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
mov    0x8(%rsp),%rcx
shl    $1,%rcx
test   %rcx,%rcx
je     <inf_store::store::CellStore::set+OFF>
mov    0x18(%rsp),%rdi
mov    %rax,%rbx
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
mov    %rbx,%rax
mov    %rax,%rdi
call   <_Unwind_Resume@plt>
int3
int3
int3
int3
int3

<inf_store::store::CellStore::report>:
push   %rbp
push   %r15
push   %r14
push   %r13
push   %r12
push   %rbx
mov    %rdi,%rax
mov    0x68(%rsi),%rcx
mov    %rcx,-0x28(%rsp)
mov    0x70(%rsi),%rcx
mov    %rcx,-0x18(%rsp)
mov    0x78(%rsi),%rcx
mov    %rcx,-0x8(%rsp)
movups 0x160(%rsi),%xmm1
movdqu 0x170(%rsi),%xmm0
mov    0x190(%rsi),%rcx
mov    %rcx,-0x10(%rsp)
mov    0xf0(%rsi),%rcx
mov    %rcx,-0x20(%rsp)
mov    0x100(%rsi),%rcx
mov    %rcx,-0x30(%rsp)
mov    0x118(%rsi),%rbx
mov    0x130(%rsi),%r15
mov    0x148(%rsi),%rcx
mov    %rcx,-0x38(%rsp)
mov    0x2278(%rsi),%rcx
test   %rcx,%rcx
je     <inf_store::store::CellStore::report+OFF>
mov    0x2270(%rsi),%rbp
lea    (%rcx,%rcx,8),%r13
shl    $0x5,%r13
add    %rbp,%r13
xor    %r10d,%r10d
xor    %r11d,%r11d
jmp    <inf_store::store::CellStore::report+OFF>
nop
xor    %r12d,%r12d
add    %r12,%r10
mov    0x8(%rbp),%rcx
mov    0x38(%rbp),%r9
mov    %rcx,%rdi
imul   %r8,%rdi
add    %r10,%rdi
mov    %r9,%r10
sub    0x18(%rbp),%rcx
mov    0x68(%rbp),%r14
add    0x30(%rbp),%rcx
imul   %r8,%rcx
sub    0x48(%rbp),%r9
add    0x60(%rbp),%r9
imul   %rdx,%r9
add    %r14,%r11
add    %rcx,%r11
add    %r9,%r11
add    0x98(%rbp),%r11
add    0xa0(%rbp),%r11
sub    0x78(%rbp),%r11
add    $0x120,%rbp
imul   %rdx,%r10
add    %r14,%r10
add    %rdi,%r10
cmp    %r13,%rbp
je     <inf_store::store::CellStore::report+OFF>
cmpl   $0x1,0x0(%rbp)
jne    <inf_store::store::CellStore::report+OFF>
mov    0x90(%rbp),%r14
mov    $0x348,%edx
mov    $0x2c8,%r8d
test   %r14,%r14
je     <inf_store::store::CellStore::report+OFF>
mov    0x88(%rbp),%rcx
cmp    $0x5,%r14
jae    <inf_store::store::CellStore::report+OFF>
xor    %edi,%edi
xor    %r12d,%r12d
jmp    <inf_store::store::CellStore::report+OFF>
data16 data16 cs nopw 0x0(%rax,%rax,1)
mov    0x90(%rbp),%r14
mov    $0x288,%edx
mov    $0x208,%r8d
test   %r14,%r14
je     <inf_store::store::CellStore::report+OFF>
mov    0x88(%rbp),%r9
cmp    $0x5,%r14
jae    <inf_store::store::CellStore::report+OFF>
xor    %edi,%edi
xor    %r12d,%r12d
jmp    <inf_store::store::CellStore::report+OFF>
mov    %r14d,%r9d
and    $0x3,%r9d
mov    $0x4,%edi
cmove  %rdi,%r9
mov    %r14,%rdi
sub    %r9,%rdi
pxor   %xmm2,%xmm2
mov    %rdi,%r12
mov    %rcx,%r9
pxor   %xmm3,%xmm3
nopw   0x0(%rax,%rax,1)
movq   0x18(%r9),%xmm4
movq   (%r9),%xmm5
punpcklqdq %xmm4,%xmm5
movq   0x48(%r9),%xmm4
movq   0x30(%r9),%xmm6
punpcklqdq %xmm4,%xmm6
psllq  $0x2,%xmm5
paddq  %xmm5,%xmm3
psllq  $0x2,%xmm6
paddq  %xmm6,%xmm2
add    $0x60,%r9
add    $0xfffffffffffffffc,%r12
jne    <inf_store::store::CellStore::report+OFF>
paddq  %xmm3,%xmm2
pshufd $0xee,%xmm2,%xmm3
paddq  %xmm2,%xmm3
movq   %xmm3,%r12
sub    %rdi,%r14
lea    (%rdi,%rdi,2),%rdi
lea    (%rcx,%rdi,8),%rcx
nopl   0x0(%rax,%rax,1)
mov    (%rcx),%rdi
lea    (%r12,%rdi,4),%r12
add    $0x18,%rcx
dec    %r14
jne    <inf_store::store::CellStore::report+OFF>
jmp    <inf_store::store::CellStore::report+OFF>
mov    %r14d,%ecx
and    $0x3,%ecx
mov    $0x4,%edi
cmove  %rdi,%rcx
mov    %r14,%rdi
sub    %rcx,%rdi
pxor   %xmm2,%xmm2
mov    %rdi,%r12
mov    %r9,%rcx
pxor   %xmm3,%xmm3
nopl   0x0(%rax,%rax,1)
movq   0x18(%rcx),%xmm4
movq   (%rcx),%xmm5
punpcklqdq %xmm4,%xmm5
movq   0x48(%rcx),%xmm4
movq   0x30(%rcx),%xmm6
punpcklqdq %xmm4,%xmm6
psllq  $0x2,%xmm5
paddq  %xmm5,%xmm3
psllq  $0x2,%xmm6
paddq  %xmm6,%xmm2
add    $0x60,%rcx
add    $0xfffffffffffffffc,%r12
jne    <inf_store::store::CellStore::report+OFF>
paddq  %xmm3,%xmm2
pshufd $0xee,%xmm2,%xmm3
paddq  %xmm2,%xmm3
movq   %xmm3,%r12
sub    %rdi,%r14
lea    (%rdi,%rdi,2),%rcx
lea    (%r9,%rcx,8),%rcx
data16 data16 cs nopw 0x0(%rax,%rax,1)
mov    (%rcx),%rdi
lea    (%r12,%rdi,4),%r12
add    $0x18,%rcx
dec    %r14
jne    <inf_store::store::CellStore::report+OFF>
jmp    <inf_store::store::CellStore::report+OFF>
xor    %r11d,%r11d
xor    %r10d,%r10d
mov    -0x18(%rsp),%r14
mov    %r14,%rcx
mov    -0x28(%rsp),%r12
sub    %r12,%rcx
shl    $0x4,%r15
mov    -0x38(%rsp),%rdx
lea    (%rdx,%rdx,2),%rdx
add    -0x30(%rsp),%rbx
add    %r15,%rbx
lea    (%rbx,%rdx,8),%rdx
mov    0x2778(%rsi),%rdi
mov    0x210(%rsi),%r8
shl    $0x4,%r8
add    $0x2020,%r8
xor    %r9d,%r9d
cmpq   $0x0,0x2738(%rsi)
lea    (%rdi,%rdi,8),%rsi
setne  %r9b
shl    $0xd,%r9d
mov    %r12,(%rax)
mov    %rcx,0x8(%rax)
mov    %r14,0x10(%rax)
mov    %rsi,0x18(%rax)
mov    %r8,0x20(%rax)
mov    %r9,0x28(%rax)
movups %xmm1,0x30(%rax)
mov    -0x20(%rsp),%rcx
mov    %rcx,0x40(%rax)
pshufd $0x4e,%xmm0,%xmm0
movdqu %xmm0,0x48(%rax)
mov    %rdx,0x58(%rax)
movq   $0x0,0x60(%rax)
mov    %r10,0x68(%rax)
mov    %r11,0x70(%rax)
mov    -0x8(%rsp),%rcx
mov    %rcx,0x78(%rax)
mov    -0x10(%rsp),%rcx
mov    %rcx,0x80(%rax)
pop    %rbx
pop    %r12
pop    %r13
pop    %r14
pop    %r15
pop    %rbp
ret
int3
int3
int3
int3
int3
int3
int3
int3
int3
int3

<core::ptr::drop_in_place<inf_store::store::CellStore>>:
push   %r15
push   %r14
push   %r13
push   %r12
push   %rbx
mov    %rdi,%rbx
mov    0x38(%rdi),%r14
mov    0x40(%rdi),%r15
test   %r15,%r15
je     <core::ptr::drop_in_place<inf_store::store::CellStore>+OFF>
shl    $0x4,%r15
xor    %r12d,%r12d
mov    0x0(%rip),%r13        # <munmap@GLIBC_2.2.5>
jmp    <core::ptr::drop_in_place<inf_store::store::CellStore>+OFF>
nopl   0x0(%rax)
add    $0x10,%r12
cmp    %r12,%r15
je     <core::ptr::drop_in_place<inf_store::store::CellStore>+OFF>
mov    0x8(%r14,%r12,1),%rsi
test   %rsi,%rsi
je     <core::ptr::drop_in_place<inf_store::store::CellStore>+OFF>
mov    (%r14,%r12,1),%rdi
call   *%r13
jmp    <core::ptr::drop_in_place<inf_store::store::CellStore>+OFF>
cmpq   $0x0,0x18(%rbx)
je     <core::ptr::drop_in_place<inf_store::store::CellStore>+OFF>
mov    0x20(%rbx),%rdi
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
cmpq   $0x0,0x30(%rbx)
je     <core::ptr::drop_in_place<inf_store::store::CellStore>+OFF>
mov    %r14,%rdi
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
cmpq   $0x0,0x48(%rbx)
je     <core::ptr::drop_in_place<inf_store::store::CellStore>+OFF>
mov    0x50(%rbx),%rdi
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
cmpq   $0x0,0x2760(%rbx)
je     <core::ptr::drop_in_place<inf_store::store::CellStore>+OFF>
mov    0x2758(%rbx),%rdi
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
cmpq   $0x0,0x2770(%rbx)
je     <core::ptr::drop_in_place<inf_store::store::CellStore>+OFF>
mov    0x2768(%rbx),%rdi
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
cmpq   $0x0,0x210(%rbx)
je     <core::ptr::drop_in_place<inf_store::store::CellStore>+OFF>
mov    0x218(%rbx),%rdi
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
mov    0x2738(%rbx),%rdi
test   %rdi,%rdi
je     <core::ptr::drop_in_place<inf_store::store::CellStore>+OFF>
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
lea    0x80(%rbx),%rdi
call   <core::ptr::drop_in_place<inf_store::doc::DocStore>>
add    $0x2268,%rbx
mov    %rbx,%rdi
pop    %rbx
pop    %r12
pop    %r13
pop    %r14
pop    %r15
jmp    <core::ptr::drop_in_place<inf_store::index_maint::imp::CellIndexes>>
int3
int3
int3
int3
int3
int3
int3


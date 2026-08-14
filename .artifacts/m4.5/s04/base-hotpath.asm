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
lea    0x0(%rip),%rcx        # <anon.6eae8eab5b5001bf886bb5a904615194.336.llvm.4750593345494487467>
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
lea    0x0(%rip),%rdx        # <anon.6eae8eab5b5001bf886bb5a904615194.336.llvm.4750593345494487467+OFF>
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
lea    0x0(%rip),%rcx        # <anon.6eae8eab5b5001bf886bb5a904615194.336.llvm.4750593345494487467>
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
lea    0x0(%rip),%rdi        # <anon.a93dd16d3da8856dffc16e8afc15beb9.322.llvm.3655386565944175712+OFF>
lea    0x0(%rip),%rdx        # <anon.6eae8eab5b5001bf886bb5a904615194.336.llvm.4750593345494487467+OFF>
mov    $0x5d,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.6eae8eab5b5001bf886bb5a904615194.336.llvm.4750593345494487467+OFF>
mov    %rax,%rdi
mov    %r8,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.6eae8eab5b5001bf886bb5a904615194.336.llvm.4750593345494487467+OFF>
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
lea    0x0(%rip),%rcx        # <anon.6eae8eab5b5001bf886bb5a904615194.336.llvm.4750593345494487467>
mov    %rax,%rdx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdi        # <anon.6eae8eab5b5001bf886bb5a904615194.347.llvm.4750593345494487467>
lea    0x0(%rip),%rdx        # <anon.6eae8eab5b5001bf886bb5a904615194.348.llvm.4750593345494487467>
mov    $0x15,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rax        # <anon.6eae8eab5b5001bf886bb5a904615194.356.llvm.4750593345494487467>
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
mov    0x2298(%r15),%rdx
mov    0x22b0(%r15),%rcx
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
mov    0x2290(%r15),%rcx
mov    %rcx,0x30(%rsp)
movd   %eax,%xmm0
punpcklbw %xmm0,%xmm0
pshuflw $0x0,%xmm0,%xmm0
pshufd $0x44,%xmm0,%xmm0
movdqa %xmm0,0x50(%rsp)
mov    0x22a8(%r15),%r13
mov    0x22a0(%r15),%rbp
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
lea    0x0(%rip),%rdi        # <anon.a93dd16d3da8856dffc16e8afc15beb9.322.llvm.3655386565944175712+OFF>
lea    0x0(%rip),%rdx        # <anon.a93dd16d3da8856dffc16e8afc15beb9.323.llvm.3655386565944175712+OFF>
mov    $0x63,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x10(%r12),%rsi
lea    0x0(%rip),%rcx        # <anon.6eae8eab5b5001bf886bb5a904615194.336.llvm.4750593345494487467>
mov    %r12,%rdi
mov    0x10(%rsp),%rdx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.6eae8eab5b5001bf886bb5a904615194.336.llvm.4750593345494487467+OFF>
mov    %r13,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.a93dd16d3da8856dffc16e8afc15beb9.323.llvm.3655386565944175712+OFF>
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
incq   0x22c8(%rbx)
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
incq   0x22d0(%rbx)
xor    %eax,%eax
mov    %rcx,%rdx
add    $0x20,%rsp
pop    %rbx
ret
lea    0x0(%rip),%rdi        # <anon.a93dd16d3da8856dffc16e8afc15beb9.322.llvm.3655386565944175712+OFF>
lea    0x0(%rip),%rdx        # <anon.a93dd16d3da8856dffc16e8afc15beb9.323.llvm.3655386565944175712+OFF>
mov    $0x63,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rcx        # <anon.6eae8eab5b5001bf886bb5a904615194.356.llvm.4750593345494487467+OFF>
mov    $0x2,%edi
mov    $0x5,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rcx        # <anon.6eae8eab5b5001bf886bb5a904615194.356.llvm.4750593345494487467+OFF>
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.a93dd16d3da8856dffc16e8afc15beb9.323.llvm.3655386565944175712+OFF>
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.a93dd16d3da8856dffc16e8afc15beb9.323.llvm.3655386565944175712+OFF>
mov    $0x1,%edi
mov    $0x1,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.6eae8eab5b5001bf886bb5a904615194.356.llvm.4750593345494487467+OFF>
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
sub    $0xd8,%rsp
mov    %rcx,%r10
mov    %rdx,0x70(%rsp)
mov    %rdi,0x8(%rsp)
mov    0x2298(%rsi),%rcx
mov    0x22b0(%rsi),%rdx
shr    $0x4,%rdx
dec    %rdx
mov    %rdx,%rdi
and    %r8,%rdi
mov    %rdi,%r12
shl    $0x4,%r12
lea    0xf(%r12),%rax
mov    %rcx,0x20(%rsp)
cmp    %rcx,%rax
jae    <inf_store::store::CellStore::resolve_hashed+OFF>
mov    %r9,0x38(%rsp)
lea    0x2290(%rsi),%rax
mov    %rax,0x30(%rsp)
mov    %r8,%r14
shr    $0x2a,%r14
mov    %r8,0x10(%rsp)
mov    %r8,%rax
shr    $0x39,%rax
mov    0x2290(%rsi),%rcx
mov    %rcx,0x48(%rsp)
movd   %eax,%xmm0
punpcklbw %xmm0,%xmm0
pshuflw $0x0,%xmm0,%xmm0
pshufd $0x44,%xmm0,%xmm0
movdqa %xmm0,0x90(%rsp)
mov    0x22a8(%rsi),%rax
mov    %rax,0x28(%rsp)
mov    0x22a0(%rsi),%rax
mov    %rax,0x88(%rsp)
mov    0x38(%rsi),%r9
mov    %rsi,0x18(%rsp)
mov    0x40(%rsi),%r8
xor    %ecx,%ecx
mov    %rdx,0x40(%rsp)
mov    %r14,0x68(%rsp)
mov    %r8,0x60(%rsp)
nopw   0x0(%rax,%rax,1)
mov    %rcx,0x58(%rsp)
mov    %rdi,0x50(%rsp)
mov    0x48(%rsp),%rax
movdqu (%rax,%r12,1),%xmm1
movdqa 0x90(%rsp),%xmm0
movdqa %xmm1,0xa0(%rsp)
pcmpeqb %xmm1,%xmm0
pmovmskb %xmm0,%eax
test   %eax,%eax
mov    %r12,0x78(%rsp)
jne    <inf_store::store::CellStore::resolve_hashed+OFF>
movdqa 0xa0(%rsp),%xmm0
pcmpeqb 0x0(%rip),%xmm0        # <anon.58baeeae7d5fe476157d296c9f08f803.346.llvm.5844774155465566772+OFF>
pmovmskb %xmm0,%eax
test   %eax,%eax
jne    <inf_store::store::CellStore::resolve_hashed+OFF>
mov    0x58(%rsp),%rcx
inc    %rcx
mov    0x40(%rsp),%rdx
cmp    %rdx,%rcx
ja     <inf_store::store::CellStore::resolve_hashed+OFF>
mov    0x50(%rsp),%rdi
add    %rcx,%rdi
and    %rdx,%rdi
mov    %rdi,%r12
shl    $0x4,%r12
lea    0xf(%r12),%rax
cmp    0x20(%rsp),%rax
jb     <inf_store::store::CellStore::resolve_hashed+OFF>
jmp    <inf_store::store::CellStore::resolve_hashed+OFF>
data16 data16 data16 data16 cs nopw 0x0(%rax,%rax,1)
lea    -0x1(%rbx),%eax
and    %ebx,%eax
test   %ax,%ax
je     <inf_store::store::CellStore::resolve_hashed+OFF>
mov    %eax,%ebx
tzcnt  %ebx,%edi
or     %r12,%rdi
cmp    0x28(%rsp),%rdi
jae    <inf_store::store::CellStore::resolve_hashed+OFF>
mov    0x88(%rsp),%rax
mov    (%rax,%rdi,8),%rax
mov    %rax,%rcx
shr    $0x30,%rcx
xor    %r14d,%ecx
test   $0x7fff,%ecx
jne    <inf_store::store::CellStore::resolve_hashed+OFF>
mov    %rax,%r11
movabs $0xffffffffffff,%rcx
and    %rcx,%r11
mov    %r11,%rdi
shr    $0x15,%rdi
cmp    %r8,%rdi
jae    <inf_store::store::CellStore::resolve_hashed+OFF>
shl    $0x4,%edi
and    $0x1fffff,%eax
lea    0x8(%rax),%rdx
mov    0x8(%r9,%rdi,1),%rcx
cmp    %rcx,%rdx
ja     <inf_store::store::CellStore::resolve_hashed+OFF>
add    %r9,%rdi
mov    (%rdi),%rbp
movzbl 0x0(%rbp,%rax,1),%edi
movzbl 0x1(%rbp,%rax,1),%edx
movzwl 0x2(%rbp,%rax,1),%esi
movzbl 0x4(%rbp,%rax,1),%r13d
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
add    %rax,%rbp
add    %rbp,%rdi
mov    0x70(%rsp),%rsi
mov    %r10,%rdx
mov    %r9,%r14
mov    %r11,0x80(%rsp)
mov    %r10,%r12
call   *0x0(%rip)        # <bcmp@GLIBC_2.2.5>
mov    %r12,%r10
mov    0x80(%rsp),%rcx
mov    %r14,%r9
mov    0x68(%rsp),%r14
mov    0x60(%rsp),%r8
mov    0x78(%rsp),%r12
test   %eax,%eax
jne    <inf_store::store::CellStore::resolve_hashed+OFF>
add    %r10,%r13
test   $0x1,%r15b
je     <inf_store::store::CellStore::resolve_hashed+OFF>
cmp    $0xc,%r13
jbe    <inf_store::store::CellStore::resolve_hashed+OFF>
mov    0x8(%rbp),%edi
movabs $0x431bde82d7b634db,%rdx
mov    0x38(%rsp),%rax
mul    %rdx
movzbl 0xc(%rbp),%eax
shl    $0x20,%rax
or     %rdi,%rax
shr    $0x12,%rdx
cmp    %rax,%rdx
jae    <inf_store::store::CellStore::resolve_hashed+OFF>
mov    %rcx,%r12
mov    0x18(%rsp),%rcx
movzbl 0x2289(%rcx),%eax
test   %eax,%eax
je     <inf_store::store::CellStore::resolve_hashed+OFF>
cmp    $0x1,%eax
jne    <inf_store::store::CellStore::resolve_hashed+OFF>
or     $0xc,%r15b
mov    %r15b,0x0(%rbp)
jmp    <inf_store::store::CellStore::resolve_hashed+OFF>
mov    0x8(%rsp),%rax
xor    %ecx,%ecx
jmp    <inf_store::store::CellStore::resolve_hashed+OFF>
movabs $0x9e3779b97f4a7c15,%rax
add    0x2278(%rcx),%rax
mov    %rax,0x2278(%rcx)
mov    0x2270(%rcx),%rcx
test   %rcx,%rcx
je     <inf_store::store::CellStore::resolve_hashed+OFF>
mov    0x10(%rsp),%rdx
mov    %edx,%r8d
shr    $0xb,%r8d
and    $0x7ff,%r8d
mov    %rdx,%rdi
shr    $0x16,%rdi
and    $0x7ff,%edi
mov    %rdx,%rsi
shr    $0x21,%rsi
and    $0x7ff,%esi
and    $0x7ff,%edx
movzbl (%rcx,%rdx,1),%r14d
movzbl 0x800(%rcx,%r8,1),%ebx
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
mov    %rdx,%r15
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
mov    0x8(%rsp),%rax
mov    %r12,0x8(%rax)
mov    %r13,0x10(%rax)
mov    $0x1,%ecx
jmp    <inf_store::store::CellStore::resolve_hashed+OFF>
lea    0xb8(%rsp),%rbx
mov    %rbx,%rdi
mov    %r9,%rsi
mov    %r8,%rdx
mov    %r13,%r8
mov    %rcx,%r15
call   <inf_store::doc::payload_of>
mov    0x30(%rsp),%rdi
mov    0x10(%rsp),%rsi
mov    %r15,%rdx
call   <inf_store::index::Index<M>::remove>
mov    0x18(%rsp),%r14
mov    %r14,%rdi
mov    %r15,%rsi
mov    %r13,%rdx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x80(%r14),%rdi
mov    %rbx,%rsi
call   <inf_store::doc::DocStore::release>
incq   0x22d8(%r14)
mov    0x22e8(%r14),%rax
cmp    $0x1,%rax
adc    $0xffffffffffffffff,%rax
mov    %rax,0x22e8(%r14)
xor    %ecx,%ecx
mov    0x8(%rsp),%rax
mov    %rcx,(%rax)
add    $0xd8,%rsp
pop    %rbx
pop    %r12
pop    %r13
pop    %r14
pop    %r15
pop    %rbp
ret
lea    0x0(%rip),%rdi        # <anon.a93dd16d3da8856dffc16e8afc15beb9.322.llvm.3655386565944175712+OFF>
lea    0x0(%rip),%rdx        # <anon.a93dd16d3da8856dffc16e8afc15beb9.323.llvm.3655386565944175712+OFF>
mov    $0x63,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x1(%r14),%eax
mov    %al,(%rcx,%r15,1)
cmp    %bpl,%bl
jne    <inf_store::store::CellStore::resolve_hashed+OFF>
lea    0x1(%rbx),%eax
mov    %al,0x800(%rcx,%r8,1)
cmp    %bpl,%r9b
jne    <inf_store::store::CellStore::resolve_hashed+OFF>
lea    0x1(%r9),%eax
mov    %al,0x1000(%rcx,%rdi,1)
cmp    %r11b,%r10b
jbe    <inf_store::store::CellStore::resolve_hashed+OFF>
jmp    <inf_store::store::CellStore::resolve_hashed+OFF>
lea    0x10(%r12),%rsi
lea    0x0(%rip),%rcx        # <anon.6eae8eab5b5001bf886bb5a904615194.336.llvm.4750593345494487467>
mov    %r12,%rdi
mov    0x20(%rsp),%rdx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rcx        # <anon.6eae8eab5b5001bf886bb5a904615194.356.llvm.4750593345494487467+OFF>
mov    $0x8,%edi
mov    $0xd,%esi
mov    %r13,%rdx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.6eae8eab5b5001bf886bb5a904615194.336.llvm.4750593345494487467+OFF>
mov    0x28(%rsp),%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.a93dd16d3da8856dffc16e8afc15beb9.323.llvm.3655386565944175712+OFF>
mov    %r8,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>

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
movzbl 0x2289(%r15),%eax
mov    $0x9,%ecx
test   %eax,%eax
jne    <inf_store::store::CellStore::write_record_carrying+OFF>
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    %r14,0x38(%rsp)
lea    0x2290(%r15),%rbx
mov    0x22b0(%r15),%r14
mov    0x22b8(%r15),%rax
mov    0x22c0(%r15),%rcx
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
lea    0x2290(%r15),%rdi
mov    0x20(%rsp),%r12
mov    %r12,%rsi
mov    %rbp,%rdx
call   <inf_store::index::Index<M>::position_of>
cmp    $0x1,%rax
jne    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x22a8(%r15),%rsi
cmp    %rsi,%rdx
jae    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x22a0(%r15),%rax
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
movzbl 0x2289(%r15),%eax
mov    $0x9,%ecx
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
mov    0x2298(%r15),%rax
mov    %rax,0x8(%rsp)
test   %r14,%r14
je     <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x2290(%r15),%rax
mov    %rax,0x48(%rsp)
mov    0x22a8(%r15),%rax
mov    %rax,0x18(%rsp)
mov    0x22a0(%r15),%rax
mov    %rax,0x40(%rsp)
mov    0x38(%r15),%rbp
mov    0x40(%r15),%rax
mov    %rax,0x30(%rsp)
xor    %r13d,%r13d
lea    0x0(%rip),%rax        # <anon.6eae8eab5b5001bf886bb5a904615194.336.llvm.4750593345494487467+OFF>
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
lea    0x2290(%r15),%rbx
je     <inf_store::store::CellStore::write_record_carrying+OFF>
mov    (%rbx),%rdi
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
cmpq   $0x0,0x22a8(%r15)
mov    0x10(%rsp),%r13
je     <inf_store::store::CellStore::write_record_carrying+OFF>
mov    0x22a0(%r15),%rdi
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
movzbl 0x2289(%r15),%eax
mov    $0x9,%ecx
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
add    0x2278(%r15),%rax
mov    %rax,0x2278(%r15)
mov    0x2270(%r15),%rsi
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
lea    0x0(%rip),%rdi        # <anon.a93dd16d3da8856dffc16e8afc15beb9.322.llvm.3655386565944175712+OFF>
lea    0x0(%rip),%rdx        # <anon.a93dd16d3da8856dffc16e8afc15beb9.323.llvm.3655386565944175712+OFF>
mov    $0x63,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
lea    0x0(%rip),%rdi        # <anon.a93dd16d3da8856dffc16e8afc15beb9.322.llvm.3655386565944175712+OFF>
lea    0x0(%rip),%rdx        # <anon.a93dd16d3da8856dffc16e8afc15beb9.323.llvm.3655386565944175712+OFF>
mov    $0x63,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdi        # <anon.6eae8eab5b5001bf886bb5a904615194.347.llvm.4750593345494487467+OFF>
lea    0x0(%rip),%rdx        # <anon.6eae8eab5b5001bf886bb5a904615194.348.llvm.4750593345494487467+OFF>
mov    $0x16,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
mov    0x8(%rsp),%rax
mov    %rax,0x18(%rsp)
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
lea    0x0(%rip),%rdx        # <anon.a93dd16d3da8856dffc16e8afc15beb9.323.llvm.3655386565944175712+OFF>
mov    0x30(%rsp),%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
jmp    <inf_store::store::CellStore::write_record_carrying+OFF>
mov    %r13,0x8(%rsp)
lea    0x0(%rip),%rax        # <anon.6eae8eab5b5001bf886bb5a904615194.336.llvm.4750593345494487467+OFF>
mov    %rax,0x28(%rsp)
mov    0x8(%rsp),%rdi
mov    0x18(%rsp),%rsi
mov    0x28(%rsp),%rdx
call   *0x0(%rip)        # <_DYNAMIC+OFF>
ud2
lea    0x0(%rip),%rdx        # <anon.a93dd16d3da8856dffc16e8afc15beb9.323.llvm.3655386565944175712+OFF>
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rax        # <anon.6eae8eab5b5001bf886bb5a904615194.348.llvm.4750593345494487467+OFF>
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
mov    0x10(%rsi),%r15
lea    -0x1(%r15),%rax
mov    %r15,%rcx
xor    %rax,%rcx
cmp    %rax,%rcx
jbe    <inf_store::store::CellStore::new+OFF>
lea    -0x10000(%r15),%rax
cmp    $0x1f0000,%rax
ja     <inf_store::store::CellStore::new+OFF>
mov    %rsi,%rbx
mov    0x38(%rsi),%rbp
shr    $0x2,%r15
mov    $0x100,%eax
mov    $0x1c,%r13d
mov    $0x2e8,%r14d
nopw   0x0(%rax,%rax,1)
add    %rax,%rax
add    $0x4,%r13
add    $0x60,%r14
cmp    %r15,%rax
jb     <inf_store::store::CellStore::new+OFF>
movabs $0x555555555555555,%rax
lea    0x3(%r13),%r12
cmp    %rax,%r12
jbe    <inf_store::store::CellStore::new+OFF>
xor    %edi,%edi
mov    %r14,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
mov    %rdi,0x10(%rsp)
mov    %r14,%rdi
call   *0x0(%rip)        # <malloc@GLIBC_2.2.5>
test   %rax,%rax
je     <inf_store::store::CellStore::new+OFF>
test   %r13,%r13
mov    %rbp,0x8(%rsp)
je     <inf_store::store::CellStore::new+OFF>
xor    %ecx,%ecx
mov    0x0(%rip),%rdx        # <anon.75fd87380dfc59308f7611a9a6d5ea54.44.llvm.7440854409185213733+OFF>
movups 0x0(%rip),%xmm0        # <anon.75fd87380dfc59308f7611a9a6d5ea54.44.llvm.7440854409185213733>
mov    %rax,%rsi
data16 data16 data16 data16 cs nopw 0x0(%rax,%rax,1)
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
cmp    %rcx,%r13
jne    <inf_store::store::CellStore::new+OFF>
jmp    <inf_store::store::CellStore::new+OFF>
xor    %ecx,%ecx
lea    (%rcx,%rcx,2),%rcx
lea    (%rax,%rcx,8),%rcx
mov    $0xffffffffffffffb8,%rdx
mov    0x0(%rip),%rbp        # <anon.75fd87380dfc59308f7611a9a6d5ea54.44.llvm.7440854409185213733+OFF>
movups 0x0(%rip),%xmm0        # <anon.75fd87380dfc59308f7611a9a6d5ea54.44.llvm.7440854409185213733>
nopl   (%rax)
mov    %rbp,0x58(%rcx,%rdx,1)
movups %xmm0,0x48(%rcx,%rdx,1)
add    $0x18,%rdx
jne    <inf_store::store::CellStore::new+OFF>
mov    0x10(%rbx),%rcx
mov    %rcx,0x60(%rsp)
movups (%rbx),%xmm0
movaps %xmm0,0x50(%rsp)
mov    %r15,0xb0(%rsp)
mov    %r12,0x68(%rsp)
mov    %rax,0x70(%rsp)
mov    %r12,0x78(%rsp)
movq   $0x0,0x80(%rsp)
movq   $0x8,0x88(%rsp)
xorps  %xmm0,%xmm0
movaps %xmm0,0x90(%rsp)
movq   $0x4,0xa0(%rsp)
movq   $0x0,0xa8(%rsp)
movups %xmm0,0xb8(%rsp)
mov    0x30(%rbx),%rax
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
mov    $0x10,%r14d
cmp    $0x2,%rax
jb     <inf_store::store::CellStore::new+OFF>
dec    %rax
bsr    %rax,%rcx
not    %ecx
mov    $0xffffffffffffffff,%rax
shr    %cl,%rax
cmp    $0x10,%rax
mov    $0xf,%r14d
cmovae %rax,%r14
inc    %r14
mov    %r14,%rdi
call   *0x0(%rip)        # <malloc@GLIBC_2.2.5>
test   %rax,%rax
je     <inf_store::store::CellStore::new+OFF>
mov    %rax,%r15
mov    %rax,%rdi
mov    $0x80,%esi
mov    %r14,%rdx
call   *0x0(%rip)        # <memset@GLIBC_2.2.5>
lea    0x0(,%r14,8),%r12
mov    %r12,%rdi
call   *0x0(%rip)        # <malloc@GLIBC_2.2.5>
test   %rax,%rax
je     <inf_store::store::CellStore::new+OFF>
mov    %rax,%r13
add    $0xfffffffffffffff8,%r12
mov    %rax,%rdi
xor    %esi,%esi
mov    %r12,%rdx
call   *0x0(%rip)        # <memset@GLIBC_2.2.5>
movq   $0x0,-0x8(%r13,%r14,8)
mov    %r15,0x18(%rsp)
mov    %r14,0x20(%rsp)
mov    %r13,0x28(%rsp)
mov    %r14,0x30(%rsp)
mov    %r14,0x38(%rsp)
xorps  %xmm0,%xmm0
movups %xmm0,0x40(%rsp)
mov    0x28(%rbx),%r15
lea    -0x1(%r15),%rax
mov    %r15,%rcx
xor    %rax,%rcx
cmp    %rax,%rcx
jbe    <inf_store::store::CellStore::new+OFF>
lea    -0x10000(%r15),%rax
cmp    $0x1f0000,%rax
ja     <inf_store::store::CellStore::new+OFF>
shr    $0x2,%r15
mov    $0x100,%eax
mov    $0x1c,%r13d
mov    $0x2e8,%r14d
xchg   %ax,%ax
add    %rax,%rax
add    $0x4,%r13
add    $0x60,%r14
cmp    %r15,%rax
jb     <inf_store::store::CellStore::new+OFF>
lea    0x3(%r13),%r12
movabs $0x555555555555555,%rax
cmp    %rax,%r12
jbe    <inf_store::store::CellStore::new+OFF>
xor    %edi,%edi
mov    %r14,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
jmp    <inf_store::store::CellStore::new+OFF>
mov    %r14,%rdi
call   *0x0(%rip)        # <malloc@GLIBC_2.2.5>
test   %rax,%rax
je     <inf_store::store::CellStore::new+OFF>
mov    %rax,%rcx
test   %r13,%r13
mov    0x10(%rsp),%rax
movups 0x0(%rip),%xmm0        # <anon.75fd87380dfc59308f7611a9a6d5ea54.44.llvm.7440854409185213733>
je     <inf_store::store::CellStore::new+OFF>
xor    %edx,%edx
mov    %rcx,%rsi
xchg   %ax,%ax
mov    %rbp,0x10(%rsi)
movups %xmm0,(%rsi)
movups %xmm0,0x18(%rsi)
mov    %rbp,0x28(%rsi)
movups %xmm0,0x30(%rsi)
mov    %rbp,0x40(%rsi)
add    $0x4,%rdx
movups %xmm0,0x48(%rsi)
mov    %rbp,0x58(%rsi)
add    $0x60,%rsi
cmp    %rdx,%r13
jne    <inf_store::store::CellStore::new+OFF>
jmp    <inf_store::store::CellStore::new+OFF>
xor    %edx,%edx
lea    (%rdx,%rdx,2),%rdx
lea    (%rcx,%rdx,8),%rdx
mov    $0xffffffffffffffb8,%rsi
nop
mov    %rbp,0x58(%rdx,%rsi,1)
movups %xmm0,0x48(%rdx,%rsi,1)
add    $0x18,%rsi
jne    <inf_store::store::CellStore::new+OFF>
mov    0x28(%rbx),%rdx
mov    %rdx,0x90(%rax)
movups 0x18(%rbx),%xmm0
movups %xmm0,0x80(%rax)
xorps  %xmm0,%xmm0
movups %xmm0,0xc0(%rax)
movups %xmm0,0xe8(%rax)
movq   $0x0,0xf8(%rax)
movups %xmm0,0x160(%rax)
movups %xmm0,0x170(%rax)
movups %xmm0,0x180(%rax)
movq   $0x0,0x190(%rax)
movups %xmm0,0x110(%rax)
movups %xmm0,0x128(%rax)
movups %xmm0,0x140(%rax)
movaps 0xc0(%rsp),%xmm1
movups %xmm1,0x70(%rax)
movaps 0xb0(%rsp),%xmm1
movups %xmm1,0x60(%rax)
movaps 0xa0(%rsp),%xmm1
movups %xmm1,0x50(%rax)
movaps 0x90(%rsp),%xmm1
movups %xmm1,0x40(%rax)
movaps 0x50(%rsp),%xmm1
movaps 0x60(%rsp),%xmm2
movaps 0x70(%rsp),%xmm3
movaps 0x80(%rsp),%xmm4
movups %xmm4,0x30(%rax)
movups %xmm3,0x20(%rax)
movups %xmm2,0x10(%rax)
movups %xmm1,(%rax)
movups 0x18(%rsp),%xmm1
movups 0x28(%rsp),%xmm2
movups 0x38(%rsp),%xmm3
movups %xmm1,0x2290(%rax)
movups %xmm2,0x22a0(%rax)
movups %xmm3,0x22b0(%rax)
mov    0x48(%rsp),%rdx
mov    %rdx,0x22c0(%rax)
movq   $0x0,0x210(%rax)
movq   $0x8,0x218(%rax)
movq   $0x0,0x220(%rax)
movaps 0x0(%rip),%xmm1        # <anon.58baeeae7d5fe476157d296c9f08f803.346.llvm.5844774155465566772+OFF>
movups %xmm1,0x228(%rax)
movups %xmm1,0x238(%rax)
movups %xmm1,0x248(%rax)
movups %xmm1,0x258(%rax)
movups %xmm1,0x268(%rax)
movups %xmm1,0x278(%rax)
movups %xmm1,0x288(%rax)
movups %xmm1,0x298(%rax)
movups %xmm1,0x2a8(%rax)
movups %xmm1,0x2b8(%rax)
movups %xmm1,0x2c8(%rax)
movups %xmm1,0x2d8(%rax)
movups %xmm1,0x2e8(%rax)
movups %xmm1,0x2f8(%rax)
movups %xmm1,0x308(%rax)
movups %xmm1,0x318(%rax)
movups %xmm1,0x328(%rax)
movups %xmm1,0x338(%rax)
movups %xmm1,0x348(%rax)
movups %xmm1,0x358(%rax)
movups %xmm1,0x368(%rax)
movups %xmm1,0x378(%rax)
movups %xmm1,0x388(%rax)
movups %xmm1,0x398(%rax)
movups %xmm1,0x3a8(%rax)
movups %xmm1,0x3b8(%rax)
movups %xmm1,0x3c8(%rax)
movups %xmm1,0x3d8(%rax)
movups %xmm1,0x3e8(%rax)
movups %xmm1,0x3f8(%rax)
movups %xmm1,0x408(%rax)
movups %xmm1,0x418(%rax)
movups %xmm1,0x428(%rax)
movups %xmm1,0x438(%rax)
movups %xmm1,0x448(%rax)
movups %xmm1,0x458(%rax)
movups %xmm1,0x468(%rax)
movups %xmm1,0x478(%rax)
movups %xmm1,0x488(%rax)
movups %xmm1,0x498(%rax)
movups %xmm1,0x4a8(%rax)
movups %xmm1,0x4b8(%rax)
movups %xmm1,0x4c8(%rax)
movups %xmm1,0x4d8(%rax)
movups %xmm1,0x4e8(%rax)
movups %xmm1,0x4f8(%rax)
movups %xmm1,0x508(%rax)
movups %xmm1,0x518(%rax)
movups %xmm1,0x528(%rax)
movups %xmm1,0x538(%rax)
movups %xmm1,0x548(%rax)
movups %xmm1,0x558(%rax)
movups %xmm1,0x568(%rax)
movups %xmm1,0x578(%rax)
movups %xmm1,0x588(%rax)
movups %xmm1,0x598(%rax)
movups %xmm1,0x5a8(%rax)
movups %xmm1,0x5b8(%rax)
movups %xmm1,0x5c8(%rax)
movups %xmm1,0x5d8(%rax)
movups %xmm1,0x5e8(%rax)
movups %xmm1,0x5f8(%rax)
movups %xmm1,0x608(%rax)
movups %xmm1,0x618(%rax)
movups %xmm1,0x628(%rax)
movups %xmm1,0x638(%rax)
movups %xmm1,0x648(%rax)
movups %xmm1,0x658(%rax)
movups %xmm1,0x668(%rax)
movups %xmm1,0x678(%rax)
movups %xmm1,0x688(%rax)
movups %xmm1,0x698(%rax)
movups %xmm1,0x6a8(%rax)
movups %xmm1,0x6b8(%rax)
movups %xmm1,0x6c8(%rax)
movups %xmm1,0x6d8(%rax)
movups %xmm1,0x6e8(%rax)
movups %xmm1,0x6f8(%rax)
movups %xmm1,0x708(%rax)
movups %xmm1,0x718(%rax)
movups %xmm1,0x728(%rax)
movups %xmm1,0x738(%rax)
movups %xmm1,0x748(%rax)
movups %xmm1,0x758(%rax)
movups %xmm1,0x768(%rax)
movups %xmm1,0x778(%rax)
movups %xmm1,0x788(%rax)
movups %xmm1,0x798(%rax)
movups %xmm1,0x7a8(%rax)
movups %xmm1,0x7b8(%rax)
movups %xmm1,0x7c8(%rax)
movups %xmm1,0x7d8(%rax)
movups %xmm1,0x7e8(%rax)
movups %xmm1,0x7f8(%rax)
movups %xmm1,0x808(%rax)
movups %xmm1,0x818(%rax)
movups %xmm1,0x828(%rax)
movups %xmm1,0x838(%rax)
movups %xmm1,0x848(%rax)
movups %xmm1,0x858(%rax)
movups %xmm1,0x868(%rax)
movups %xmm1,0x878(%rax)
movups %xmm1,0x888(%rax)
movups %xmm1,0x898(%rax)
movups %xmm1,0x8a8(%rax)
movups %xmm1,0x8b8(%rax)
movups %xmm1,0x8c8(%rax)
movups %xmm1,0x8d8(%rax)
movups %xmm1,0x8e8(%rax)
movups %xmm1,0x8f8(%rax)
movups %xmm1,0x908(%rax)
movups %xmm1,0x918(%rax)
movups %xmm1,0x928(%rax)
movups %xmm1,0x938(%rax)
movups %xmm1,0x948(%rax)
movups %xmm1,0x958(%rax)
movups %xmm1,0x968(%rax)
movups %xmm1,0x978(%rax)
movups %xmm1,0x988(%rax)
movups %xmm1,0x998(%rax)
movups %xmm1,0x9a8(%rax)
movups %xmm1,0x9b8(%rax)
movups %xmm1,0x9c8(%rax)
movups %xmm1,0x9d8(%rax)
movups %xmm1,0x9e8(%rax)
movups %xmm1,0x9f8(%rax)
movups %xmm1,0xa08(%rax)
movups %xmm1,0xa18(%rax)
movups %xmm1,0xa28(%rax)
movups %xmm1,0xa38(%rax)
movups %xmm1,0xa48(%rax)
movups %xmm1,0xa58(%rax)
movups %xmm1,0xa68(%rax)
movups %xmm1,0xa78(%rax)
movups %xmm1,0xa88(%rax)
movups %xmm1,0xa98(%rax)
movups %xmm1,0xaa8(%rax)
movups %xmm1,0xab8(%rax)
movups %xmm1,0xac8(%rax)
movups %xmm1,0xad8(%rax)
movups %xmm1,0xae8(%rax)
movups %xmm1,0xaf8(%rax)
movups %xmm1,0xb08(%rax)
movups %xmm1,0xb18(%rax)
movups %xmm1,0xb28(%rax)
movups %xmm1,0xb38(%rax)
movups %xmm1,0xb48(%rax)
movups %xmm1,0xb58(%rax)
movups %xmm1,0xb68(%rax)
movups %xmm1,0xb78(%rax)
movups %xmm1,0xb88(%rax)
movups %xmm1,0xb98(%rax)
movups %xmm1,0xba8(%rax)
movups %xmm1,0xbb8(%rax)
movups %xmm1,0xbc8(%rax)
movups %xmm1,0xbd8(%rax)
movups %xmm1,0xbe8(%rax)
movups %xmm1,0xbf8(%rax)
movups %xmm1,0xc08(%rax)
movups %xmm1,0xc18(%rax)
movups %xmm1,0xc28(%rax)
movups %xmm1,0xc38(%rax)
movups %xmm1,0xc48(%rax)
movups %xmm1,0xc58(%rax)
movups %xmm1,0xc68(%rax)
movups %xmm1,0xc78(%rax)
movups %xmm1,0xc88(%rax)
movups %xmm1,0xc98(%rax)
movups %xmm1,0xca8(%rax)
movups %xmm1,0xcb8(%rax)
movups %xmm1,0xcc8(%rax)
movups %xmm1,0xcd8(%rax)
movups %xmm1,0xce8(%rax)
movups %xmm1,0xcf8(%rax)
movups %xmm1,0xd08(%rax)
movups %xmm1,0xd18(%rax)
movups %xmm1,0xd28(%rax)
movups %xmm1,0xd38(%rax)
movups %xmm1,0xd48(%rax)
movups %xmm1,0xd58(%rax)
movups %xmm1,0xd68(%rax)
movups %xmm1,0xd78(%rax)
movups %xmm1,0xd88(%rax)
movups %xmm1,0xd98(%rax)
movups %xmm1,0xda8(%rax)
movups %xmm1,0xdb8(%rax)
movups %xmm1,0xdc8(%rax)
movups %xmm1,0xdd8(%rax)
movups %xmm1,0xde8(%rax)
movups %xmm1,0xdf8(%rax)
movups %xmm1,0xe08(%rax)
movups %xmm1,0xe18(%rax)
movups %xmm1,0xe28(%rax)
movups %xmm1,0xe38(%rax)
movups %xmm1,0xe48(%rax)
movups %xmm1,0xe58(%rax)
movups %xmm1,0xe68(%rax)
movups %xmm1,0xe78(%rax)
movups %xmm1,0xe88(%rax)
movups %xmm1,0xe98(%rax)
movups %xmm1,0xea8(%rax)
movups %xmm1,0xeb8(%rax)
movups %xmm1,0xec8(%rax)
movups %xmm1,0xed8(%rax)
movups %xmm1,0xee8(%rax)
movups %xmm1,0xef8(%rax)
movups %xmm1,0xf08(%rax)
movups %xmm1,0xf18(%rax)
movups %xmm1,0xf28(%rax)
movups %xmm1,0xf38(%rax)
movups %xmm1,0xf48(%rax)
movups %xmm1,0xf58(%rax)
movups %xmm1,0xf68(%rax)
movups %xmm1,0xf78(%rax)
movups %xmm1,0xf88(%rax)
movups %xmm1,0xf98(%rax)
movups %xmm1,0xfa8(%rax)
movups %xmm1,0xfb8(%rax)
movups %xmm1,0xfc8(%rax)
movups %xmm1,0xfd8(%rax)
movups %xmm1,0xfe8(%rax)
movups %xmm1,0xff8(%rax)
movups %xmm1,0x1008(%rax)
movups %xmm1,0x1018(%rax)
movups %xmm1,0x1028(%rax)
movups %xmm1,0x1038(%rax)
movups %xmm1,0x1048(%rax)
movups %xmm1,0x1058(%rax)
movups %xmm1,0x1068(%rax)
movups %xmm1,0x1078(%rax)
movups %xmm1,0x1088(%rax)
movups %xmm1,0x1098(%rax)
movups %xmm1,0x10a8(%rax)
movups %xmm1,0x10b8(%rax)
movups %xmm1,0x10c8(%rax)
movups %xmm1,0x10d8(%rax)
movups %xmm1,0x10e8(%rax)
movups %xmm1,0x10f8(%rax)
movups %xmm1,0x1108(%rax)
movups %xmm1,0x1118(%rax)
movups %xmm1,0x1128(%rax)
movups %xmm1,0x1138(%rax)
movups %xmm1,0x1148(%rax)
movups %xmm1,0x1158(%rax)
movups %xmm1,0x1168(%rax)
movups %xmm1,0x1178(%rax)
movups %xmm1,0x1188(%rax)
movups %xmm1,0x1198(%rax)
movups %xmm1,0x11a8(%rax)
movups %xmm1,0x11b8(%rax)
movups %xmm1,0x11c8(%rax)
movups %xmm1,0x11d8(%rax)
movups %xmm1,0x11e8(%rax)
movups %xmm1,0x11f8(%rax)
movups %xmm1,0x1208(%rax)
movups %xmm1,0x1218(%rax)
movups %xmm1,0x1228(%rax)
movups %xmm1,0x1238(%rax)
movups %xmm1,0x1248(%rax)
movups %xmm1,0x1258(%rax)
movups %xmm1,0x1268(%rax)
movups %xmm1,0x1278(%rax)
movups %xmm1,0x1288(%rax)
movups %xmm1,0x1298(%rax)
movups %xmm1,0x12a8(%rax)
movups %xmm1,0x12b8(%rax)
movups %xmm1,0x12c8(%rax)
movups %xmm1,0x12d8(%rax)
movups %xmm1,0x12e8(%rax)
movups %xmm1,0x12f8(%rax)
movups %xmm1,0x1308(%rax)
movups %xmm1,0x1318(%rax)
movups %xmm1,0x1328(%rax)
movups %xmm1,0x1338(%rax)
movups %xmm1,0x1348(%rax)
movups %xmm1,0x1358(%rax)
movups %xmm1,0x1368(%rax)
movups %xmm1,0x1378(%rax)
movups %xmm1,0x1388(%rax)
movups %xmm1,0x1398(%rax)
movups %xmm1,0x13a8(%rax)
movups %xmm1,0x13b8(%rax)
movups %xmm1,0x13c8(%rax)
movups %xmm1,0x13d8(%rax)
movups %xmm1,0x13e8(%rax)
movups %xmm1,0x13f8(%rax)
movups %xmm1,0x1408(%rax)
movups %xmm1,0x1418(%rax)
movups %xmm1,0x1428(%rax)
movups %xmm1,0x1438(%rax)
movups %xmm1,0x1448(%rax)
movups %xmm1,0x1458(%rax)
movups %xmm1,0x1468(%rax)
movups %xmm1,0x1478(%rax)
movups %xmm1,0x1488(%rax)
movups %xmm1,0x1498(%rax)
movups %xmm1,0x14a8(%rax)
movups %xmm1,0x14b8(%rax)
movups %xmm1,0x14c8(%rax)
movups %xmm1,0x14d8(%rax)
movups %xmm1,0x14e8(%rax)
movups %xmm1,0x14f8(%rax)
movups %xmm1,0x1508(%rax)
movups %xmm1,0x1518(%rax)
movups %xmm1,0x1528(%rax)
movups %xmm1,0x1538(%rax)
movups %xmm1,0x1548(%rax)
movups %xmm1,0x1558(%rax)
movups %xmm1,0x1568(%rax)
movups %xmm1,0x1578(%rax)
movups %xmm1,0x1588(%rax)
movups %xmm1,0x1598(%rax)
movups %xmm1,0x15a8(%rax)
movups %xmm1,0x15b8(%rax)
movups %xmm1,0x15c8(%rax)
movups %xmm1,0x15d8(%rax)
movups %xmm1,0x15e8(%rax)
movups %xmm1,0x15f8(%rax)
movups %xmm1,0x1608(%rax)
movups %xmm1,0x1618(%rax)
movups %xmm1,0x1628(%rax)
movups %xmm1,0x1638(%rax)
movups %xmm1,0x1648(%rax)
movups %xmm1,0x1658(%rax)
movups %xmm1,0x1668(%rax)
movups %xmm1,0x1678(%rax)
movups %xmm1,0x1688(%rax)
movups %xmm1,0x1698(%rax)
movups %xmm1,0x16a8(%rax)
movups %xmm1,0x16b8(%rax)
movups %xmm1,0x16c8(%rax)
movups %xmm1,0x16d8(%rax)
movups %xmm1,0x16e8(%rax)
movups %xmm1,0x16f8(%rax)
movups %xmm1,0x1708(%rax)
movups %xmm1,0x1718(%rax)
movups %xmm1,0x1728(%rax)
movups %xmm1,0x1738(%rax)
movups %xmm1,0x1748(%rax)
movups %xmm1,0x1758(%rax)
movups %xmm1,0x1768(%rax)
movups %xmm1,0x1778(%rax)
movups %xmm1,0x1788(%rax)
movups %xmm1,0x1798(%rax)
movups %xmm1,0x17a8(%rax)
movups %xmm1,0x17b8(%rax)
movups %xmm1,0x17c8(%rax)
movups %xmm1,0x17d8(%rax)
movups %xmm1,0x17e8(%rax)
movups %xmm1,0x17f8(%rax)
movups %xmm1,0x1808(%rax)
movups %xmm1,0x1818(%rax)
movups %xmm1,0x1828(%rax)
movups %xmm1,0x1838(%rax)
movups %xmm1,0x1848(%rax)
movups %xmm1,0x1858(%rax)
movups %xmm1,0x1868(%rax)
movups %xmm1,0x1878(%rax)
movups %xmm1,0x1888(%rax)
movups %xmm1,0x1898(%rax)
movups %xmm1,0x18a8(%rax)
movups %xmm1,0x18b8(%rax)
movups %xmm1,0x18c8(%rax)
movups %xmm1,0x18d8(%rax)
movups %xmm1,0x18e8(%rax)
movups %xmm1,0x18f8(%rax)
movups %xmm1,0x1908(%rax)
movups %xmm1,0x1918(%rax)
movups %xmm1,0x1928(%rax)
movups %xmm1,0x1938(%rax)
movups %xmm1,0x1948(%rax)
movups %xmm1,0x1958(%rax)
movups %xmm1,0x1968(%rax)
movups %xmm1,0x1978(%rax)
movups %xmm1,0x1988(%rax)
movups %xmm1,0x1998(%rax)
movups %xmm1,0x19a8(%rax)
movups %xmm1,0x19b8(%rax)
movups %xmm1,0x19c8(%rax)
movups %xmm1,0x19d8(%rax)
movups %xmm1,0x19e8(%rax)
movups %xmm1,0x19f8(%rax)
movups %xmm1,0x1a08(%rax)
movups %xmm1,0x1a18(%rax)
movups %xmm1,0x1a28(%rax)
movups %xmm1,0x1a38(%rax)
movups %xmm1,0x1a48(%rax)
movups %xmm1,0x1a58(%rax)
movups %xmm1,0x1a68(%rax)
movups %xmm1,0x1a78(%rax)
movups %xmm1,0x1a88(%rax)
movups %xmm1,0x1a98(%rax)
movups %xmm1,0x1aa8(%rax)
movups %xmm1,0x1ab8(%rax)
movups %xmm1,0x1ac8(%rax)
movups %xmm1,0x1ad8(%rax)
movups %xmm1,0x1ae8(%rax)
movups %xmm1,0x1af8(%rax)
movups %xmm1,0x1b08(%rax)
movups %xmm1,0x1b18(%rax)
movups %xmm1,0x1b28(%rax)
movups %xmm1,0x1b38(%rax)
movups %xmm1,0x1b48(%rax)
movups %xmm1,0x1b58(%rax)
movups %xmm1,0x1b68(%rax)
movups %xmm1,0x1b78(%rax)
movups %xmm1,0x1b88(%rax)
movups %xmm1,0x1b98(%rax)
movups %xmm1,0x1ba8(%rax)
movups %xmm1,0x1bb8(%rax)
movups %xmm1,0x1bc8(%rax)
movups %xmm1,0x1bd8(%rax)
movups %xmm1,0x1be8(%rax)
movups %xmm1,0x1bf8(%rax)
movups %xmm1,0x1c08(%rax)
movups %xmm1,0x1c18(%rax)
movups %xmm1,0x1c28(%rax)
movups %xmm1,0x1c38(%rax)
movups %xmm1,0x1c48(%rax)
movups %xmm1,0x1c58(%rax)
movups %xmm1,0x1c68(%rax)
movups %xmm1,0x1c78(%rax)
movups %xmm1,0x1c88(%rax)
movups %xmm1,0x1c98(%rax)
movups %xmm1,0x1ca8(%rax)
movups %xmm1,0x1cb8(%rax)
movups %xmm1,0x1cc8(%rax)
movups %xmm1,0x1cd8(%rax)
movups %xmm1,0x1ce8(%rax)
movups %xmm1,0x1cf8(%rax)
movups %xmm1,0x1d08(%rax)
movups %xmm1,0x1d18(%rax)
movups %xmm1,0x1d28(%rax)
movups %xmm1,0x1d38(%rax)
movups %xmm1,0x1d48(%rax)
movups %xmm1,0x1d58(%rax)
movups %xmm1,0x1d68(%rax)
movups %xmm1,0x1d78(%rax)
movups %xmm1,0x1d88(%rax)
movups %xmm1,0x1d98(%rax)
movups %xmm1,0x1da8(%rax)
movups %xmm1,0x1db8(%rax)
movups %xmm1,0x1dc8(%rax)
movups %xmm1,0x1dd8(%rax)
movups %xmm1,0x1de8(%rax)
movups %xmm1,0x1df8(%rax)
movups %xmm1,0x1e08(%rax)
movups %xmm1,0x1e18(%rax)
movups %xmm1,0x1e28(%rax)
movups %xmm1,0x1e38(%rax)
movups %xmm1,0x1e48(%rax)
movups %xmm1,0x1e58(%rax)
movups %xmm1,0x1e68(%rax)
movups %xmm1,0x1e78(%rax)
movups %xmm1,0x1e88(%rax)
movups %xmm1,0x1e98(%rax)
movups %xmm1,0x1ea8(%rax)
movups %xmm1,0x1eb8(%rax)
movups %xmm1,0x1ec8(%rax)
movups %xmm1,0x1ed8(%rax)
movups %xmm1,0x1ee8(%rax)
movups %xmm1,0x1ef8(%rax)
movups %xmm1,0x1f08(%rax)
movups %xmm1,0x1f18(%rax)
movups %xmm1,0x1f28(%rax)
movups %xmm1,0x1f38(%rax)
movups %xmm1,0x1f48(%rax)
movups %xmm1,0x1f58(%rax)
movups %xmm1,0x1f68(%rax)
movups %xmm1,0x1f78(%rax)
movups %xmm1,0x1f88(%rax)
movups %xmm1,0x1f98(%rax)
movups %xmm1,0x1fa8(%rax)
movups %xmm1,0x1fb8(%rax)
movups %xmm1,0x1fc8(%rax)
movups %xmm1,0x1fd8(%rax)
movups %xmm1,0x1fe8(%rax)
movups %xmm1,0x1ff8(%rax)
movups %xmm1,0x2008(%rax)
movups %xmm1,0x2018(%rax)
movups %xmm1,0x2028(%rax)
movups %xmm1,0x2038(%rax)
movups %xmm1,0x2048(%rax)
movups %xmm1,0x2058(%rax)
movups %xmm1,0x2068(%rax)
movups %xmm1,0x2078(%rax)
movups %xmm1,0x2088(%rax)
movups %xmm1,0x2098(%rax)
movups %xmm1,0x20a8(%rax)
movups %xmm1,0x20b8(%rax)
movups %xmm1,0x20c8(%rax)
movups %xmm1,0x20d8(%rax)
movups %xmm1,0x20e8(%rax)
movups %xmm1,0x20f8(%rax)
movups %xmm1,0x2108(%rax)
movups %xmm1,0x2118(%rax)
movups %xmm1,0x2128(%rax)
movups %xmm1,0x2138(%rax)
movups %xmm1,0x2148(%rax)
movups %xmm1,0x2158(%rax)
movups %xmm1,0x2168(%rax)
movups %xmm1,0x2178(%rax)
movups %xmm1,0x2188(%rax)
movups %xmm1,0x2198(%rax)
movups %xmm1,0x21a8(%rax)
movups %xmm1,0x21b8(%rax)
movups %xmm1,0x21c8(%rax)
movups %xmm1,0x21d8(%rax)
movups %xmm1,0x21e8(%rax)
movups %xmm1,0x21f8(%rax)
movups %xmm1,0x2208(%rax)
movups %xmm1,0x2218(%rax)
movups %xmm0,0x2248(%rax)
movups %xmm0,0x2238(%rax)
movups %xmm0,0x2228(%rax)
movq   $0x0,0x2258(%rax)
movabs $0xffffff00ffffff,%rdx
mov    %rdx,0x2260(%rax)
movups %xmm0,0x2268(%rax)
movups %xmm0,0x22f8(%rax)
movups %xmm0,0x22e8(%rax)
movups %xmm0,0x22d8(%rax)
movups %xmm0,0x22c8(%rax)
mov    0x8(%rsp),%rdx
mov    %rdx,0x2278(%rax)
movq   $0x0,0x2280(%rax)
movw   $0x0,0x2288(%rax)
mov    %r12,0x98(%rax)
mov    %rcx,0xa0(%rax)
mov    %r12,0xa8(%rax)
movq   $0x0,0xb0(%rax)
movq   $0x8,0xb8(%rax)
movq   $0x4,0xd0(%rax)
movq   $0x0,0xd8(%rax)
mov    %r15,0xe0(%rax)
movq   $0x0,0x100(%rax)
movq   $0x1,0x108(%rax)
movq   $0x1,0x120(%rax)
movq   $0x8,0x138(%rax)
movq   $0x8,0x150(%rax)
movq   $0x0,0x158(%rax)
movq   $0x1,0x198(%rax)
movups 0x60(%rbx),%xmm0
movups %xmm0,0x200(%rax)
movups 0x50(%rbx),%xmm0
movups %xmm0,0x1f0(%rax)
movups 0x40(%rbx),%xmm0
movups %xmm0,0x1e0(%rax)
movups (%rbx),%xmm0
movups 0x10(%rbx),%xmm1
movups 0x20(%rbx),%xmm2
movups 0x30(%rbx),%xmm3
movups %xmm3,0x1d0(%rax)
movups %xmm2,0x1c0(%rax)
movups %xmm1,0x1b0(%rax)
movups %xmm0,0x1a0(%rax)
add    $0xd8,%rsp
pop    %rbx
pop    %r12
pop    %r13
pop    %r14
pop    %r15
pop    %rbp
ret
lea    0x0(%rip),%rdi        # <anon.75fd87380dfc59308f7611a9a6d5ea54.42.llvm.7440854409185213733>
lea    0x0(%rip),%rdx        # <anon.75fd87380dfc59308f7611a9a6d5ea54.43.llvm.7440854409185213733>
mov    $0x69,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdi        # <anon.75fd87380dfc59308f7611a9a6d5ea54.42.llvm.7440854409185213733>
lea    0x0(%rip),%rdx        # <anon.75fd87380dfc59308f7611a9a6d5ea54.43.llvm.7440854409185213733>
mov    $0x69,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
jmp    <inf_store::store::CellStore::new+OFF>
mov    $0x8,%edi
mov    %r14,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
mov    $0x1,%edi
mov    %r14,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
jmp    <inf_store::store::CellStore::new+OFF>
mov    $0x8,%edi
mov    %r12,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
ud2
mov    $0x8,%edi
jmp    <inf_store::store::CellStore::new+OFF>
mov    %rax,%rbx
mov    %r15,%rdi
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
int3
int3
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
cmp    $0x9,%rax
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
cmp    $0x9,%rax
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
incq   0x22e8(%rax)
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
mov    $0x22f8,%eax
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
mov    0x22e8(%rcx),%rax
cmp    $0x1,%rax
adc    $0xffffffffffffffff,%rax
mov    %rax,0x22e8(%rcx)
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
lea    0x0(%rip),%rdi        # <anon.a93dd16d3da8856dffc16e8afc15beb9.322.llvm.3655386565944175712+OFF>
lea    0x0(%rip),%rdx        # <anon.a93dd16d3da8856dffc16e8afc15beb9.323.llvm.3655386565944175712+OFF>
mov    $0x63,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rcx        # <anon.6eae8eab5b5001bf886bb5a904615194.356.llvm.4750593345494487467+OFF>
mov    $0x5,%edi
mov    $0x8,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
jmp    <inf_store::store::CellStore::set+OFF>
lea    0x0(%rip),%rcx        # <anon.6eae8eab5b5001bf886bb5a904615194.356.llvm.4750593345494487467+OFF>
mov    $0x8,%edi
mov    $0xd,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rcx        # <anon.6eae8eab5b5001bf886bb5a904615194.356.llvm.4750593345494487467+OFF>
mov    $0x2,%edi
mov    $0x5,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rcx        # <anon.6eae8eab5b5001bf886bb5a904615194.356.llvm.4750593345494487467+OFF>
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdi        # <anon.58baeeae7d5fe476157d296c9f08f803.1.llvm.5844774155465566772+OFF>
lea    0x0(%rip),%rdx        # <anon.6eae8eab5b5001bf886bb5a904615194.356.llvm.4750593345494487467+OFF>
mov    $0x10,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.a93dd16d3da8856dffc16e8afc15beb9.323.llvm.3655386565944175712+OFF>
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.6eae8eab5b5001bf886bb5a904615194.356.llvm.4750593345494487467+OFF>
xor    %edi,%edi
xor    %esi,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.6eae8eab5b5001bf886bb5a904615194.356.llvm.4750593345494487467+OFF>
xor    %edi,%edi
xor    %esi,%esi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.6eae8eab5b5001bf886bb5a904615194.356.llvm.4750593345494487467+OFF>
jmp    <inf_store::store::CellStore::set+OFF>
lea    0x0(%rip),%rdx        # <anon.6eae8eab5b5001bf886bb5a904615194.356.llvm.4750593345494487467+OFF>
jmp    <inf_store::store::CellStore::set+OFF>
mov    $0x1,%edi
mov    %rbp,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
lea    0x0(%rip),%rdx        # <anon.6eae8eab5b5001bf886bb5a904615194.356.llvm.4750593345494487467+OFF>
jmp    <inf_store::store::CellStore::set+OFF>
lea    0x0(%rip),%rdx        # <anon.6eae8eab5b5001bf886bb5a904615194.356.llvm.4750593345494487467+OFF>
mov    %r12,%rsi
call   *0x0(%rip)        # <_DYNAMIC+OFF>
ud2
lea    0x0(%rip),%rdx        # <anon.a93dd16d3da8856dffc16e8afc15beb9.323.llvm.3655386565944175712+OFF>
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
cmpq   $0x0,0x2298(%rbx)
je     <core::ptr::drop_in_place<inf_store::store::CellStore>+OFF>
mov    0x2290(%rbx),%rdi
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
cmpq   $0x0,0x22a8(%rbx)
je     <core::ptr::drop_in_place<inf_store::store::CellStore>+OFF>
mov    0x22a0(%rbx),%rdi
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
cmpq   $0x0,0x210(%rbx)
je     <core::ptr::drop_in_place<inf_store::store::CellStore>+OFF>
mov    0x218(%rbx),%rdi
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
mov    0x2270(%rbx),%rdi
test   %rdi,%rdi
je     <core::ptr::drop_in_place<inf_store::store::CellStore>+OFF>
call   *0x0(%rip)        # <free@GLIBC_2.2.5>
sub    $0xffffffffffffff80,%rbx
mov    %rbx,%rdi
pop    %rbx
pop    %r12
pop    %r13
pop    %r14
pop    %r15
jmp    <core::ptr::drop_in_place<inf_store::doc::DocStore>>
int3
int3
int3
int3
int3
int3


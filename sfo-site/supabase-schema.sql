create table public.reports (
  id uuid primary key default gen_random_uuid(),
  name text not null,
  category text not null check (category in ('Minigiochi', 'Anime', 'Film', 'Quiz', 'Disegno', 'Musica')),
  event_date date not null,
  event_time time not null,
  duration integer not null check (duration in (15, 30, 45, 60, 75, 90, 105, 120, 150, 180, 240)),
  attendees integer not null check (attendees >= 0),
  new_members integer not null check (new_members >= 0 and new_members <= attendees),
  new_member_names text not null default '',
  responsible text not null,
  notes text not null default '',
  clip_recorded boolean not null default false,
  clip_registrar text not null default '',
  flop boolean not null default false,
  created_at timestamptz not null default now(),
  constraint clip_registrar_required check (not clip_recorded or length(trim(clip_registrar)) > 0)
);

alter table public.reports enable row level security;

drop policy if exists "staff can read reports" on public.reports;
drop policy if exists "staff can create reports" on public.reports;
drop policy if exists "staff can update reports" on public.reports;
drop policy if exists "staff can delete reports" on public.reports;

create policy "staff can read reports"
on public.reports for select
to authenticated
using ((select auth.jwt() -> 'app_metadata' ->> 'role') = 'staff');

create policy "staff can create reports"
on public.reports for insert
to authenticated
with check ((select auth.jwt() -> 'app_metadata' ->> 'role') = 'staff');

create policy "staff can update reports"
on public.reports for update
to authenticated
using ((select auth.jwt() -> 'app_metadata' ->> 'role') = 'staff')
with check ((select auth.jwt() -> 'app_metadata' ->> 'role') = 'staff');

create policy "staff can delete reports"
on public.reports for delete
to authenticated
using ((select auth.jwt() -> 'app_metadata' ->> 'role') = 'staff');

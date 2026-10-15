-- Second database for the multi-connection explorer.
create database analytics;
\connect analytics

create schema events;

create table events.page_views (
    id bigint generated always as identity primary key,
    path text not null,
    referrer text,
    viewed_at timestamptz not null default now()
);

insert into events.page_views (path, referrer, viewed_at)
select (array['/', '/products', '/cart', '/checkout'])[1 + g % 4],
       case when g % 3 = 0 then 'https://example.com' end,
       now() - (g || ' minutes')::interval
from generate_series(1, 50000) g;

create table public.daily_kpis (
    day date primary key,
    visitors integer not null,
    conversions integer not null
);

insert into public.daily_kpis
select current_date - g, 1000 + g * 7 % 300, 30 + g % 25
from generate_series(0, 89) g;

create view public.v_conversion_rate as
select day, round(conversions::numeric / visitors, 4) as rate
from public.daily_kpis;

create function public.conversion_rate(p_day date) returns numeric
    language sql stable
    as $$ select round(conversions::numeric / visitors, 4) from public.daily_kpis where day = p_day $$;

analyze;

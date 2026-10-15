-- Sample data: 20,000 customers and 1,000,000 orders.
create type order_status as enum ('pending', 'paid', 'refunded', 'cancelled');

create table customers (
    id uuid primary key default gen_random_uuid(),
    email text not null,
    full_name text not null,
    country char(2) not null,
    created_at timestamptz not null default now()
);

insert into customers (email, full_name, country, created_at)
select 'customer' || g || '@example.com',
       'Customer ' || g,
       (array['ES', 'PT', 'FR', 'DE', 'IT'])[1 + g % 5],
       now() - (g || ' minutes')::interval
from generate_series(1, 20000) g;

create table orders (
    id bigint generated always as identity primary key,
    customer_id uuid not null references customers (id),
    status order_status not null default 'pending',
    total numeric(10, 2) not null,
    currency char(3) not null default 'EUR',
    created_at timestamptz not null default now(),
    shipping jsonb
);

with cs as (select array_agg(id) as ids from customers)
insert into orders (customer_id, status, total, created_at, shipping)
select cs.ids[1 + g % 20000],
       (array['pending', 'paid', 'paid', 'paid', 'refunded', 'cancelled']::order_status[])[1 + g % 6],
       round((random() * 300)::numeric, 2),
       now() - (g || ' seconds')::interval,
       jsonb_build_object(
           'city', (array['Madrid', 'Lisboa', 'Paris', 'Berlin', 'Roma'])[1 + g % 5],
           'zip', lpad((g % 99999)::text, 5, '0'))
from cs, generate_series(1, 1000000) g;

create index orders_customer_id_idx on orders (customer_id);
create index orders_created_at_idx on orders (created_at desc);
analyze;

-- Extra structure for the explorer: CHECK, trigger, enum arrays,
-- views and a table without a primary key.
alter table orders add constraint orders_total_positive check (total >= 0);
alter table orders add column updated_at timestamptz not null default now();

create function set_updated_at() returns trigger language plpgsql as $$
begin
    new.updated_at := now();
    return new;
end $$;

create trigger orders_set_updated_at before update on orders
    for each row execute function set_updated_at();

create type user_role as enum ('admin', 'editor', 'viewer');

create table profiles (
    user_id uuid primary key references customers (id) on delete cascade,
    created_at timestamptz not null default now(),
    roles user_role[] not null default '{viewer}'
);
comment on table profiles is 'Profile and roles of each customer';

insert into profiles (user_id, roles)
select id, case when row_number() over () = 1 then '{admin}'::user_role[] else '{viewer}' end
from customers
limit 50;

create view v_daily_sales as
select date_trunc('day', created_at)::date as day, count(*) as orders, sum(total) as revenue
from orders
where status = 'paid'
group by 1;

-- No primary key: it can be viewed, but not edited row by row.
create table audit_log (
    at timestamptz not null default now(),
    actor text,
    action text not null
);
insert into audit_log (actor, action) values ('admin', 'seed'), (null, 'vacuum');

-- For the driver's write tests.
create table playground_items (
    id bigint generated always as identity primary key,
    name text not null,
    quantity integer not null default 0,
    note text
);
insert into playground_items (name, quantity) values ('screw', 100), ('nut', 250);

analyze;

-- Datos de ejemplo: 20 000 clientes y 1 000 000 de pedidos.
create type order_status as enum ('pending', 'paid', 'refunded', 'cancelled');

create table customers (
    id uuid primary key default gen_random_uuid(),
    email text not null,
    full_name text not null,
    country char(2) not null,
    created_at timestamptz not null default now()
);

insert into customers (email, full_name, country, created_at)
select 'cliente' || g || '@example.com',
       'Cliente ' || g,
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

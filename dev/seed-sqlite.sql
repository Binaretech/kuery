-- Sample SQLite database for Kuery: a small shop. Build it with
-- `cargo run -p kuery-sqlite --example seed` (writes dev/shop.sqlite).
pragma foreign_keys = on;

create table customers (
    id integer primary key,
    email text not null unique,
    full_name text not null,
    country text not null,
    created_at datetime not null default current_timestamp
);

create table products (
    id integer primary key,
    sku text not null unique,
    name text not null,
    price real not null check (price >= 0)
);

create table orders (
    id integer primary key,
    customer_id integer not null references customers (id) on delete cascade,
    status text not null default 'pending'
        check (status in ('pending', 'paid', 'refunded', 'cancelled')),
    total real not null default 0,
    created_at datetime not null default current_timestamp,
    note text
);

create table order_lines (
    order_id integer not null references orders (id) on delete cascade,
    line integer not null,
    product_id integer not null references products (id),
    quantity integer not null default 1,
    primary key (order_id, line)
);

create index orders_by_customer on orders (customer_id);
create index lines_by_product on order_lines (product_id);

create view paid_orders as
select o.id, c.email, o.total, o.created_at
from orders o join customers c on c.id = o.customer_id
where o.status = 'paid';

create trigger order_lines_total after insert on order_lines
begin
    update orders
    set total = total + new.quantity * (select price from products where id = new.product_id)
    where id = new.order_id;
end;

with recursive n(i) as (select 1 union all select i + 1 from n where i < 2000)
insert into customers (id, email, full_name, country, created_at)
select i,
       'customer' || i || '@example.com',
       'Customer ' || i,
       case i % 5 when 0 then 'ES' when 1 then 'PT' when 2 then 'FR' when 3 then 'DE' else 'IT' end,
       datetime('2026-01-01', '+' || i || ' hours')
from n;

with recursive n(i) as (select 1 union all select i + 1 from n where i < 120)
insert into products (id, sku, name, price)
select i, 'SKU-' || printf('%04d', i), 'Product ' || i, round(2 + (i * 37 % 300) + (i % 100) / 100.0, 2)
from n;

with recursive n(i) as (select 1 union all select i + 1 from n where i < 20000)
insert into orders (id, customer_id, status, created_at, note)
select i,
       1 + i * 7 % 2000,
       case i % 6 when 0 then 'pending' when 4 then 'refunded' when 5 then 'cancelled' else 'paid' end,
       datetime('2026-01-01', '+' || (i * 13) || ' minutes'),
       case when i % 11 = 0 then 'Gift wrap, please' end
from n;

with recursive n(i) as (select 1 union all select i + 1 from n where i < 50000)
insert into order_lines (order_id, line, product_id, quantity)
select 1 + (i - 1) / 2 % 20000 + (i - 1) / 40000 * 0,
       1 + (i - 1) % 2 + (i - 1) / 40000 * 2,
       1 + i * 13 % 120,
       1 + i % 4
from n;

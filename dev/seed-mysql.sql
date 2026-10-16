-- Sample MySQL database for Kuery: a small shop in the `shop` database,
-- and a `kuery` user without a password (development only).
create user if not exists 'kuery'@'%' identified by '';
grant all privileges on shop.* to 'kuery'@'%';
grant process on *.* to 'kuery'@'%';
-- To read the bodies of functions and procedures it did not create.
grant show_routine on *.* to 'kuery'@'%';

use shop;
set session cte_max_recursion_depth = 100000;

create table customers (
    id int unsigned auto_increment primary key,
    email varchar(120) not null unique,
    full_name varchar(120) not null,
    country char(2) not null,
    created_at datetime not null default current_timestamp
) comment 'People who buy';

create table products (
    id int unsigned auto_increment primary key,
    sku varchar(20) not null unique,
    name varchar(120) not null,
    price decimal(10, 2) not null,
    constraint price_not_negative check (price >= 0)
);

create table orders (
    id bigint unsigned auto_increment primary key,
    customer_id int unsigned not null,
    status enum('pending', 'paid', 'refunded', 'cancelled') not null default 'pending',
    total decimal(10, 2) not null default 0,
    created_at datetime not null default current_timestamp,
    shipping json null,
    note text null,
    constraint orders_customer foreign key (customer_id) references customers (id) on delete cascade,
    index orders_by_created (created_at)
);

create table order_lines (
    order_id bigint unsigned not null,
    line smallint unsigned not null,
    product_id int unsigned not null,
    quantity smallint unsigned not null default 1,
    primary key (order_id, line),
    constraint lines_order foreign key (order_id) references orders (id) on delete cascade,
    constraint lines_product foreign key (product_id) references products (id)
);

create view paid_orders as
select o.id, c.email, o.total, o.created_at
from orders o join customers c on c.id = o.customer_id
where o.status = 'paid';

delimiter //
create trigger order_lines_total after insert on order_lines for each row
begin
    update orders
    set total = total + new.quantity * (select price from products where id = new.product_id)
    where id = new.order_id;
end//

create function order_count(customer int unsigned) returns int
reads sql data
begin
    return (select count(*) from orders where customer_id = customer);
end//

create procedure refund(order_id bigint unsigned)
begin
    update orders set status = 'refunded' where id = order_id;
end//
delimiter ;

insert into customers (id, email, full_name, country, created_at)
with recursive n(i) as (select 1 union all select i + 1 from n where i < 2000)
select i, concat('customer', i, '@example.com'), concat('Customer ', i),
       elt(1 + i % 5, 'ES', 'PT', 'FR', 'DE', 'IT'),
       timestamp('2026-01-01') + interval i hour
from n;

insert into products (id, sku, name, price)
with recursive n(i) as (select 1 union all select i + 1 from n where i < 120)
select i, concat('SKU-', lpad(i, 4, '0')), concat('Product ', i), 2 + (i * 37 % 300) + (i % 100) / 100
from n;

insert into orders (id, customer_id, status, created_at, shipping, note)
with recursive n(i) as (select 1 union all select i + 1 from n where i < 20000)
select i, 1 + i * 7 % 2000,
       elt(1 + i % 6, 'pending', 'paid', 'paid', 'paid', 'refunded', 'cancelled'),
       timestamp('2026-01-01') + interval (i * 13) minute,
       json_object('city', elt(1 + i % 5, 'Madrid', 'Lisboa', 'Paris', 'Berlin', 'Roma')),
       if(i % 11 = 0, 'Gift wrap, please', null)
from n;

insert into order_lines (order_id, line, product_id, quantity)
with recursive n(i) as (select 1 union all select i + 1 from n where i < 40000)
select 1 + (i - 1) div 2, 1 + (i - 1) % 2, 1 + i * 13 % 120, 1 + i % 4
from n;

analyze table customers, products, orders, order_lines;

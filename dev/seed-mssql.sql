-- Sample SQL Server database for Kuery: the small shop of seed-mysql.sql in
-- the `shop` database (schema dbo), plus a `sales` schema with one object of
-- each kind the catalog shows. Run by the mssql-seed service as sa
-- (development only); it starts the database over each time.
use master;
go

if db_id('shop') is not null
begin
    alter database shop set single_user with rollback immediate;
    drop database shop;
end;
go

create database shop;
go

-- Readers see the last committed rows instead of waiting for writers' locks
-- (how Azure SQL Database behaves, and what the conformance suite expects).
alter database shop set read_committed_snapshot on with rollback immediate;
go

use shop;
go

create table dbo.customers (
    id int primary key,
    email varchar(120) not null unique,
    full_name nvarchar(120) not null,
    country char(2) not null,
    created_at datetime2(0) not null default sysdatetime()
);
exec sp_addextendedproperty 'MS_Description', 'People who buy', 'schema', 'dbo', 'table', 'customers';

create table dbo.products (
    id int primary key,
    sku varchar(20) not null unique,
    name nvarchar(120) not null,
    price decimal(10, 2) not null,
    constraint price_not_negative check (price >= 0)
);

create table dbo.orders (
    id bigint primary key,
    customer_id int not null,
    status varchar(10) not null default 'pending'
        constraint orders_status check (status in ('pending', 'paid', 'refunded', 'cancelled')),
    total decimal(10, 2) not null default 0,
    created_at datetime2(0) not null default sysdatetime(),
    shipping nvarchar(max) null constraint orders_shipping_json check (isjson(shipping) = 1),
    note nvarchar(max) null,
    constraint orders_customer foreign key (customer_id) references dbo.customers (id) on delete cascade
);
create index orders_by_created on dbo.orders (created_at);

create table dbo.order_lines (
    order_id bigint not null,
    line smallint not null,
    product_id int not null,
    quantity smallint not null default 1,
    primary key (order_id, line),
    constraint lines_order foreign key (order_id) references dbo.orders (id) on delete cascade,
    constraint lines_product foreign key (product_id) references dbo.products (id)
);
go

create view dbo.paid_orders as
select o.id, c.email, o.total, o.created_at
from dbo.orders o join dbo.customers c on c.id = o.customer_id
where o.status = 'paid';
go

create trigger dbo.order_lines_total on dbo.order_lines after insert as
begin
    set nocount on;
    update o
    set total = o.total + t.amount
    from dbo.orders o
    join (
        select i.order_id, sum(i.quantity * p.price) as amount
        from inserted i join dbo.products p on p.id = i.product_id
        group by i.order_id
    ) t on t.order_id = o.id;
end;
go

create function dbo.order_count(@customer int) returns int as
begin
    return (select count(*) from dbo.orders where customer_id = @customer);
end;
go

create procedure dbo.refund @order_id bigint as
begin
    update dbo.orders set status = 'refunded' where id = @order_id;
end;
go

-- Numbers 1 to 40000 for the rows below.
with n as (
    select top (40000) row_number() over (order by (select null)) as i
    from sys.all_objects a cross join sys.all_objects b
)
select cast(i as int) as i into #n from n;

insert into dbo.customers (id, email, full_name, country, created_at)
select i, concat('customer', i, '@example.com'), concat(N'Customer ', i),
       choose(1 + i % 5, 'ES', 'PT', 'FR', 'DE', 'IT'),
       dateadd(hour, i, cast('2026-01-01' as datetime2(0)))
from #n where i <= 2000;

insert into dbo.products (id, sku, name, price)
select i, concat('SKU-', right(concat('0000', i), 4)), concat(N'Product ', i),
       2 + (i * 37 % 300) + (i % 100) / 100.0
from #n where i <= 120;

insert into dbo.orders (id, customer_id, status, created_at, shipping, note)
select i, 1 + i * 7 % 2000,
       choose(1 + i % 6, 'pending', 'paid', 'paid', 'paid', 'refunded', 'cancelled'),
       dateadd(minute, i * 13, cast('2026-01-01' as datetime2(0))),
       concat(N'{"city": "', choose(1 + i % 5, 'Madrid', 'Lisboa', 'Paris', 'Berlin', 'Roma'), N'"}'),
       iif(i % 11 = 0, N'Gift wrap, please', null)
from #n where i <= 20000;

insert into dbo.order_lines (order_id, line, product_id, quantity)
select 1 + (i - 1) / 2, 1 + (i - 1) % 2, 1 + i * 13 % 120, 1 + i % 4
from #n;

drop table #n;
go

-- One of each kind in a second schema.
create schema sales;
go

create type sales.money_amount from decimal(12, 2) not null;
go

create table sales.quotas (
    country char(2) primary key,
    amount sales.money_amount,
    changed_at datetime2(0) null
);
insert into sales.quotas (country, amount)
values ('ES', 50000), ('PT', 20000), ('FR', 60000), ('DE', 80000), ('IT', 45000);
go

create trigger sales.quotas_changed on sales.quotas after update as
begin
    set nocount on;
    update q set changed_at = sysdatetime()
    from sales.quotas q join inserted i on i.country = q.country;
end;
go

create view sales.revenue_by_country as
select c.country, sum(o.total) as revenue
from dbo.orders o join dbo.customers c on c.id = o.customer_id
where o.status = 'paid'
group by c.country;
go

create function sales.quota_reached(@country char(2)) returns bit as
begin
    return iif(
        (select revenue from sales.revenue_by_country where country = @country)
            >= (select amount from sales.quotas where country = @country),
        1, 0);
end;
go

create procedure sales.raise_quota @country char(2), @by decimal(12, 2) as
begin
    update sales.quotas set amount = amount + @by where country = @country;
end;
go

update statistics dbo.customers;
update statistics dbo.products;
update statistics dbo.orders;
update statistics dbo.order_lines;
go

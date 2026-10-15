-- connection: local-dev
-- Paid orders and new customers per month, last 12 months.
select date_trunc('month', o.created_at)::date as month,
       count(*) as orders,
       sum(o.total) as revenue,
       round(avg(o.total), 2) as average_ticket
from orders o
where o.status = 'paid'
  and o.created_at >= now() - interval '12 months'
group by 1
order by 1 desc;

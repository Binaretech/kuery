-- connection: local-analytics
select day, visitors, conversions, round(conversions::numeric / visitors, 4) as rate
from public.daily_kpis
order by day desc
limit 30;

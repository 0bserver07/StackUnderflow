import {
  BarChart,
  Bar,
  XAxis,
  YAxis,
  CartesianGrid,
  Tooltip,
  ResponsiveContainer,
  Legend,
} from 'recharts'
import type { DailyData } from '../../types/api'

interface DailyCostChartProps {
  dailyStats: Record<string, DailyData>
}

export default function DailyCostChart({ dailyStats }: DailyCostChartProps) {
  if (!dailyStats || Object.keys(dailyStats).length === 0) return null

  const data = Object.entries(dailyStats)
    .map(([date, d]) => {
      let inputCost = 0
      let outputCost = 0
      let cacheCost = 0

      if (d.cost.by_model) {
        for (const modelCost of Object.values(d.cost.by_model)) {
          inputCost += modelCost.input_cost
          outputCost += modelCost.output_cost
          cacheCost += modelCost.cache_creation_cost + modelCost.cache_read_cost
        }
      }

      return {
        date,
        input_cost: inputCost,
        output_cost: outputCost,
        cache_cost: cacheCost,
      }
    })
    .sort((a, b) => a.date.localeCompare(b.date))

  return (
    <div className="bg-gray-100/70 dark:bg-gray-800/50 rounded-lg p-4 border border-gray-200 dark:border-gray-800">
      <h3 className="text-sm font-medium text-gray-700 dark:text-gray-300 mb-3">Daily Cost</h3>
      <ResponsiveContainer width="100%" height={280}>
        <BarChart data={data}>
          <CartesianGrid strokeDasharray="3 3" stroke="#374151" />
          <XAxis
            dataKey="date"
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
          />
          <YAxis
            tick={{ fontSize: 10, fill: '#9CA3AF' }}
            tickLine={{ stroke: '#4B5563' }}
            axisLine={{ stroke: '#4B5563' }}
            tickFormatter={(v) => `$${v.toFixed(2)}`}
          />
          <Tooltip
            contentStyle={{
              backgroundColor: '#1F2937',
              border: '1px solid #374151',
              borderRadius: '6px',
              fontSize: '12px',
            }}
            labelStyle={{ color: '#D1D5DB' }}
            formatter={(value: number, name: string) => {
              const labels: Record<string, string> = {
                input_cost: 'Input Cost',
                output_cost: 'Output Cost',
                cache_cost: 'Cache Cost',
              }
              return [`$${value.toFixed(4)}`, labels[name] ?? name]
            }}
          />
          <Legend
            wrapperStyle={{ fontSize: '11px', color: '#9CA3AF' }}
            formatter={(value: string) => {
              const labels: Record<string, string> = {
                input_cost: 'Input',
                output_cost: 'Output',
                cache_cost: 'Cache',
              }
              return labels[value] ?? value
            }}
          />
          <Bar
            dataKey="input_cost"
            stackId="cost"
            fill="#818CF8"
            radius={[0, 0, 0, 0]}
            name="input_cost"
          />
          <Bar
            dataKey="output_cost"
            stackId="cost"
            fill="#34D399"
            radius={[0, 0, 0, 0]}
            name="output_cost"
          />
          <Bar
            dataKey="cache_cost"
            stackId="cost"
            fill="#F59E0B"
            radius={[4, 4, 0, 0]}
            name="cache_cost"
          />
        </BarChart>
      </ResponsiveContainer>
    </div>
  )
}

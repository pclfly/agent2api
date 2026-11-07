/** Credit values are optional measurements, never an implicit zero. */
export type CreditMeasurement = {
  credit?: number | null
  creditRequests?: number
  creditEstimated?: boolean
  success?: number
}
export function creditReading(item: CreditMeasurement): { text: string; estimated: boolean; partial: boolean; measured: number; successful: number } {
  const measured = Math.max(0, item.creditRequests ?? 0)
  const successful = Math.max(0, item.success ?? 0)
  const known = typeof item.credit === 'number' && Number.isFinite(item.credit)
  const estimated = known && item.creditEstimated === true
  const partial = known && (item.creditRequests === undefined || item.success === undefined || measured < successful)
  const amount = known ? item.credit!.toFixed(6).replace(/\.?0+$/, '') || '0' : '—'
  return { text: `${estimated ? '≈' : ''}${amount}${partial ? '*' : ''}`, estimated, partial, measured, successful }
}

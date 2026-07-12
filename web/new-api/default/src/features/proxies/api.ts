/*
Copyright (C) 2023-2026 QuantumNous

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU Affero General Public License as
published by the Free Software Foundation, either version 3 of the
License, or (at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU Affero General Public License for more details.

You should have received a copy of the GNU Affero General Public License
along with this program. If not, see <https://www.gnu.org/licenses/>.

For commercial licensing, please contact support@quantumnous.com
*/
import { api } from '@/lib/api'

import type { ApiResponse, Proxy, ProxyFormData } from './types'

export async function listProxies(): Promise<ApiResponse<Proxy[]>> {
  const res = await api.get('/api/proxy/')
  return res.data
}

export async function getProxy(id: number): Promise<ApiResponse<Proxy>> {
  const res = await api.get(`/api/proxy/${id}`)
  return res.data
}

export async function createProxy(
  data: ProxyFormData
): Promise<ApiResponse<Proxy>> {
  const res = await api.post('/api/proxy/', data)
  return res.data
}

export async function updateProxy(
  data: ProxyFormData & { id: number }
): Promise<ApiResponse<Proxy>> {
  const res = await api.put('/api/proxy/', data)
  return res.data
}

export async function deleteProxy(id: number): Promise<ApiResponse> {
  const res = await api.delete(`/api/proxy/${id}`)
  return res.data
}

export type ProxyTestResult = {
  success: boolean
  message: string
  latency_ms?: number
  ip_address?: string
  city?: string
  region?: string
  country?: string
  country_code?: string
}

export type ProxyQualityCheckItem = {
  target: string
  status: string
  http_status?: number
  latency_ms?: number
  message?: string
  cf_ray?: string
}

export type ProxyQualityCheckResult = {
  proxy_id: number
  score: number
  grade: string
  summary: string
  exit_ip?: string
  country?: string
  country_code?: string
  base_latency_ms?: number
  passed_count: number
  warn_count: number
  failed_count: number
  challenge_count: number
  checked_at: number
  items: ProxyQualityCheckItem[]
}

/** Sub2API-style connectivity test (exit IP + latency). */
export async function testProxy(
  id: number
): Promise<ApiResponse<ProxyTestResult>> {
  const res = await api.post(`/api/proxy/${id}/test`, undefined, {
    // probes can take a few seconds
    timeout: 30000,
  })
  return res.data
}

/** Sub2API-style quality check against common AI API endpoints. */
export async function qualityCheckProxy(
  id: number
): Promise<ApiResponse<ProxyQualityCheckResult>> {
  const res = await api.post(`/api/proxy/${id}/quality-check`, undefined, {
    timeout: 60000,
  })
  return res.data
}

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
import { useQuery } from '@tanstack/react-query'
import { useMemo, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { toast } from 'sonner'

import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'

import {
  listProxies,
  qualityCheckProxy,
  testProxy,
  type ProxyQualityCheckResult,
  type ProxyTestResult,
} from '../api'
import { ERROR_MESSAGES, PROXY_STATUS } from '../constants'
import type { Proxy } from '../types'
import { useProxies } from './proxies-provider'

export function ProxiesTable() {
  const { t } = useTranslation()
  const { refreshTrigger, setOpen, setCurrentRow } = useProxies()
  const [filter, setFilter] = useState('')
  const [busyId, setBusyId] = useState<number | null>(null)
  const [lastTest, setLastTest] = useState<
    Record<number, ProxyTestResult | undefined>
  >({})
  const [lastQuality, setLastQuality] = useState<
    Record<number, ProxyQualityCheckResult | undefined>
  >({})

  const { data, isLoading, isFetching } = useQuery({
    queryKey: ['proxies', refreshTrigger],
    queryFn: async () => {
      const result = await listProxies()
      if (!result?.success) {
        throw new Error(result?.message || t(ERROR_MESSAGES.LOAD_FAILED))
      }
      return Array.isArray(result.data) ? result.data : []
    },
    retry: 1,
  })

  const proxies = useMemo(() => {
    const items = Array.isArray(data) ? data : []
    const q = filter.trim().toLowerCase()
    if (!q) return items
    return items.filter((p) => {
      return (
        String(p.id).includes(q) ||
        (p.name || '').toLowerCase().includes(q) ||
        (p.url || '').toLowerCase().includes(q)
      )
    })
  }, [data, filter])

  const runTest = async (proxy: Proxy) => {
    setBusyId(proxy.id)
    try {
      const res = await testProxy(proxy.id)
      if (!res.success || !res.data) {
        toast.error(res.message || t('Proxy test failed'))
        return
      }
      setLastTest((prev) => ({ ...prev, [proxy.id]: res.data }))
      if (res.data.success) {
        const bits = [
          res.data.ip_address,
          res.data.country || res.data.country_code,
          res.data.latency_ms != null ? `${res.data.latency_ms}ms` : null,
        ].filter(Boolean)
        toast.success(
          bits.length
            ? `${t('Proxy OK')}: ${bits.join(' · ')}`
            : res.data.message || t('Proxy OK')
        )
      } else {
        toast.error(res.data.message || t('Proxy test failed'))
      }
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('Proxy test failed'))
    } finally {
      setBusyId(null)
    }
  }

  const runQuality = async (proxy: Proxy) => {
    setBusyId(proxy.id)
    try {
      const res = await qualityCheckProxy(proxy.id)
      if (!res.success || !res.data) {
        toast.error(res.message || t('Quality check failed'))
        return
      }
      setLastQuality((prev) => ({ ...prev, [proxy.id]: res.data }))
      const q = res.data
      toast.message(`${t('Quality')} ${q.grade} (${q.score})`, {
        description: [
          q.summary,
          q.exit_ip ? `IP ${q.exit_ip}` : null,
          q.base_latency_ms ? `${q.base_latency_ms}ms` : null,
        ]
          .filter(Boolean)
          .join(' · '),
      })
    } catch (err) {
      toast.error(
        err instanceof Error ? err.message : t('Quality check failed')
      )
    } finally {
      setBusyId(null)
    }
  }

  return (
    <div className='flex h-full min-h-0 flex-col gap-3'>
      <div className='flex items-center gap-2'>
        <Input
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder={t('Filter by name, URL, or ID...')}
          className='max-w-sm'
        />
        {(isLoading || isFetching) && (
          <span className='text-muted-foreground text-xs'>
            {t('Loading...')}
          </span>
        )}
      </div>
      <div className='min-h-0 flex-1 overflow-auto rounded-md border'>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead className='w-[70px]'>{t('ID')}</TableHead>
              <TableHead>{t('Name')}</TableHead>
              <TableHead>{t('URL')}</TableHead>
              <TableHead className='w-[90px]'>{t('Status')}</TableHead>
              <TableHead>{t('Exit / Quality')}</TableHead>
              <TableHead className='w-[280px]'>{t('Actions')}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {!isLoading && proxies.length === 0 ? (
              <TableRow>
                <TableCell
                  colSpan={6}
                  className='text-muted-foreground h-24 text-center'
                >
                  {t('No Proxies Found')}
                </TableCell>
              </TableRow>
            ) : (
              proxies.map((proxy: Proxy) => {
                const enabled = proxy.status === PROXY_STATUS.ENABLED
                const test = lastTest[proxy.id]
                const quality = lastQuality[proxy.id]
                const busy = busyId === proxy.id
                return (
                  <TableRow
                    key={proxy.id}
                    className={enabled ? undefined : 'opacity-60'}
                  >
                    <TableCell className='font-mono tabular-nums'>
                      {proxy.id}
                    </TableCell>
                    <TableCell className='font-medium'>{proxy.name}</TableCell>
                    <TableCell className='text-muted-foreground max-w-[280px] truncate font-mono text-xs'>
                      {proxy.url}
                    </TableCell>
                    <TableCell>
                      {enabled ? t('Enabled') : t('Disabled')}
                    </TableCell>
                    <TableCell className='text-muted-foreground text-xs'>
                      {quality ? (
                        <div className='space-y-0.5'>
                          <div>
                            {t('Grade')} {quality.grade} ({quality.score}) ·{' '}
                            {quality.base_latency_ms
                              ? `${quality.base_latency_ms}ms`
                              : '—'}
                          </div>
                          <div className='truncate'>
                            {quality.exit_ip || '—'}
                            {quality.country ? ` · ${quality.country}` : ''}
                          </div>
                        </div>
                      ) : test ? (
                        <div className='space-y-0.5'>
                          <div>
                            {test.success ? t('OK') : t('Fail')}
                            {test.latency_ms != null
                              ? ` · ${test.latency_ms}ms`
                              : ''}
                          </div>
                          <div className='truncate'>
                            {test.ip_address || test.message || '—'}
                            {test.country ? ` · ${test.country}` : ''}
                          </div>
                        </div>
                      ) : (
                        proxy.remark || '—'
                      )}
                    </TableCell>
                    <TableCell>
                      <div className='flex flex-wrap gap-1'>
                        <Button
                          size='sm'
                          variant='outline'
                          disabled={busy}
                          onClick={() => void runTest(proxy)}
                        >
                          {t('Test')}
                        </Button>
                        <Button
                          size='sm'
                          variant='outline'
                          disabled={busy}
                          onClick={() => void runQuality(proxy)}
                        >
                          {t('Quality')}
                        </Button>
                        <Button
                          size='sm'
                          variant='outline'
                          disabled={busy}
                          onClick={() => {
                            setCurrentRow(proxy)
                            setOpen('update')
                          }}
                        >
                          {t('Edit')}
                        </Button>
                        <Button
                          size='sm'
                          variant='ghost'
                          disabled={busy}
                          onClick={() => {
                            setCurrentRow(proxy)
                            setOpen('delete')
                          }}
                        >
                          {t('Delete')}
                        </Button>
                      </div>
                    </TableCell>
                  </TableRow>
                )
              })
            )}
          </TableBody>
        </Table>
      </div>
    </div>
  )
}

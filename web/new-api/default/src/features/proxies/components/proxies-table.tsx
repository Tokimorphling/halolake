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

import { listProxies } from '../api'
import { ERROR_MESSAGES, PROXY_STATUS } from '../constants'
import type { Proxy } from '../types'
import { useProxies } from './proxies-provider'

export function ProxiesTable() {
  const { t } = useTranslation()
  const { refreshTrigger, setOpen, setCurrentRow } = useProxies()
  const [filter, setFilter] = useState('')

  const { data, isLoading, isFetching, isError, error } = useQuery({
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

  if (isError && error) {
    // one toast per failed fetch via query error boundary pattern
  }

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
          <span className='text-muted-foreground text-xs'>{t('Loading...')}</span>
        )}
      </div>
      <div className='min-h-0 flex-1 overflow-auto rounded-md border'>
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead className='w-[80px]'>{t('ID')}</TableHead>
              <TableHead>{t('Name')}</TableHead>
              <TableHead>{t('URL')}</TableHead>
              <TableHead className='w-[100px]'>{t('Status')}</TableHead>
              <TableHead>{t('Remark')}</TableHead>
              <TableHead className='w-[120px]'>{t('Actions')}</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {!isLoading && proxies.length === 0 ? (
              <TableRow>
                <TableCell colSpan={6} className='text-muted-foreground h-24 text-center'>
                  {t('No Proxies Found')}
                </TableCell>
              </TableRow>
            ) : (
              proxies.map((proxy: Proxy) => {
                const enabled = proxy.status === PROXY_STATUS.ENABLED
                return (
                  <TableRow
                    key={proxy.id}
                    className={enabled ? undefined : 'opacity-60'}
                  >
                    <TableCell className='font-mono tabular-nums'>
                      {proxy.id}
                    </TableCell>
                    <TableCell className='font-medium'>{proxy.name}</TableCell>
                    <TableCell className='text-muted-foreground max-w-[320px] truncate font-mono text-xs'>
                      {proxy.url}
                    </TableCell>
                    <TableCell>
                      {enabled ? t('Enabled') : t('Disabled')}
                    </TableCell>
                    <TableCell className='text-muted-foreground text-sm'>
                      {proxy.remark || '—'}
                    </TableCell>
                    <TableCell>
                      <div className='flex gap-1'>
                        <Button
                          size='sm'
                          variant='outline'
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

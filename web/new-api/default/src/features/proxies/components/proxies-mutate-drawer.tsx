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
import { type FormEvent, useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { toast } from 'sonner'

import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { Switch } from '@/components/ui/switch'
import { Textarea } from '@/components/ui/textarea'

import { createProxy, updateProxy } from '../api'
import { ERROR_MESSAGES, PROXY_STATUS, SUCCESS_MESSAGES } from '../constants'
import type { Proxy } from '../types'
import { useProxies } from './proxies-provider'

function isValidProxyUrl(value: string): boolean {
  const v = value.trim().toLowerCase()
  return (
    v.startsWith('http://') ||
    v.startsWith('https://') ||
    v.startsWith('socks5://') ||
    v.startsWith('socks5h://')
  )
}

type ProxiesMutateDrawerProps = {
  open: boolean
  onOpenChange: (open: boolean) => void
  currentRow?: Proxy
}

export function ProxiesMutateDrawer({
  open,
  onOpenChange,
  currentRow,
}: ProxiesMutateDrawerProps) {
  const { t } = useTranslation()
  const isUpdate = !!currentRow
  const { triggerRefresh } = useProxies()
  const [isSubmitting, setIsSubmitting] = useState(false)
  const [name, setName] = useState('')
  const [url, setUrl] = useState('')
  const [enabled, setEnabled] = useState(true)
  const [remark, setRemark] = useState('')

  useEffect(() => {
    if (!open) return
    if (isUpdate && currentRow) {
      setName(currentRow.name || '')
      setUrl(currentRow.url || '')
      setEnabled(currentRow.status === PROXY_STATUS.ENABLED)
      setRemark(currentRow.remark || '')
    } else {
      setName('')
      setUrl('')
      setEnabled(true)
      setRemark('')
    }
  }, [open, isUpdate, currentRow])

  const onSubmit = async (event: FormEvent) => {
    event.preventDefault()
    const trimmedName = name.trim()
    const trimmedUrl = url.trim()
    if (!trimmedName || !trimmedUrl) {
      toast.error(t('Name and URL are required'))
      return
    }
    if (!isValidProxyUrl(trimmedUrl)) {
      toast.error(
        t('Invalid proxy URL (allowed: http, https, socks5, socks5h)')
      )
      return
    }
    setIsSubmitting(true)
    try {
      const payload = {
        name: trimmedName,
        url: trimmedUrl,
        status: enabled ? PROXY_STATUS.ENABLED : PROXY_STATUS.DISABLED,
        remark: remark.trim(),
      }
      const result =
        isUpdate && currentRow
          ? await updateProxy({ ...payload, id: currentRow.id })
          : await createProxy(payload)
      if (result.success) {
        toast.success(
          t(
            isUpdate
              ? SUCCESS_MESSAGES.PROXY_UPDATED
              : SUCCESS_MESSAGES.PROXY_CREATED
          )
        )
        onOpenChange(false)
        triggerRefresh()
      } else {
        toast.error(result.message || t(ERROR_MESSAGES.SAVE_FAILED))
      }
    } catch (err) {
      toast.error(
        err instanceof Error ? err.message : t(ERROR_MESSAGES.SAVE_FAILED)
      )
    } finally {
      setIsSubmitting(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className='sm:max-w-lg'>
        <DialogHeader>
          <DialogTitle>
            {isUpdate ? t('Update Proxy') : t('Add Proxy')}
          </DialogTitle>
          <DialogDescription>
            {t(
              'Upstream egress proxy for channels. socks5 is upgraded to socks5h (remote DNS).'
            )}
          </DialogDescription>
        </DialogHeader>
        <form id='proxy-form' onSubmit={(e) => void onSubmit(e)} className='space-y-4'>
          <div className='space-y-2'>
            <Label htmlFor='proxy-name'>{t('Name')}</Label>
            <Input
              id='proxy-name'
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder={t('us-east')}
              autoComplete='off'
            />
          </div>
          <div className='space-y-2'>
            <Label htmlFor='proxy-url'>{t('URL')}</Label>
            <Input
              id='proxy-url'
              value={url}
              onChange={(e) => setUrl(e.target.value)}
              placeholder='socks5h://warp-socks:9091'
              autoComplete='off'
            />
            <p className='text-muted-foreground text-xs'>
              {t(
                'http, https, socks5, or socks5h. Prefer socks5h for remote DNS. No auth example: socks5h://warp-socks:9091'
              )}
            </p>
          </div>
          <div className='flex items-center justify-between gap-4'>
            <div className='space-y-0.5'>
              <Label htmlFor='proxy-enabled'>{t('Enabled')}</Label>
              <p className='text-muted-foreground text-xs'>
                {t('Disabled proxies are not resolved for channel traffic.')}
              </p>
            </div>
            <Switch
              id='proxy-enabled'
              checked={enabled}
              onCheckedChange={setEnabled}
            />
          </div>
          <div className='space-y-2'>
            <Label htmlFor='proxy-remark'>{t('Remark')}</Label>
            <Textarea
              id='proxy-remark'
              rows={2}
              value={remark}
              onChange={(e) => setRemark(e.target.value)}
              placeholder={t('Optional notes')}
            />
          </div>
        </form>
        <DialogFooter>
          <Button
            type='button'
            variant='outline'
            onClick={() => onOpenChange(false)}
            disabled={isSubmitting}
          >
            {t('Cancel')}
          </Button>
          <Button type='submit' form='proxy-form' disabled={isSubmitting}>
            {isSubmitting ? t('Saving...') : t('Save')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

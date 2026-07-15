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
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { FileJson, KeyRound, Upload } from 'lucide-react'
import { useMemo, useRef, useState } from 'react'
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
import { Label } from '@/components/ui/label'
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { Textarea } from '@/components/ui/textarea'
import { listProxies } from '@/features/proxies/api'
import { PROXY_STATUS } from '@/features/proxies/constants'
import type { Proxy } from '@/features/proxies/types'
import { cn } from '@/lib/utils'

import {
  getGroups,
  importAuthJson,
  importAuthUpload,
  type AuthImportMethod,
} from '../../api'
import { channelsQueryKeys } from '../../lib'

type ImportDataDialogProps = {
  open: boolean
  onOpenChange: (open: boolean) => void
}

/** Source family for file / API import */
type ImportMode = 'auto' | 'sub2api-data' | 'cliproxy' | 'codex-session'

/**
 * OpenAI / Codex authorization method (sub2api-aligned).
 * - file: multi-file / drag-drop
 * - refresh_token: manual RT (one per line, batch)
 * - mobile_refresh_token: mobile OAuth client RT (one per line, batch)
 * - codex_session: Codex JSON / AT paste (batch lines or JSON)
 * - codex_pat: Codex Personal Access Token (at-*, batch)
 */
type AuthMethod =
  | 'file'
  | 'refresh_token'
  | 'mobile_refresh_token'
  | 'codex_session'
  | 'codex_pat'

function apiAuthMethod(method: AuthMethod): AuthImportMethod {
  return method === 'file' ? 'auto' : method
}

function fileHelpText(mode: ImportMode, t: (key: string) => string): string {
  if (mode === 'sub2api-data') {
    return t('JSON (.json) — sub2api export (proxies + accounts)')
  }
  if (mode === 'cliproxy') {
    return t(
      'One or more CLIProxyAPI *.json auth files (type: codex/claude/gemini/xai)'
    )
  }
  if (mode === 'codex-session') {
    return t('Codex / sub2api session auth JSON or access token')
  }
  return t(
    'Auto: sub2api-data export, CLIProxyAPI auth files (incl. xAI), or Codex session. Drag & drop JSON files here.'
  )
}

function collectJsonFiles(list: FileList | File[] | null): File[] {
  if (!list) return []
  return [...list].filter((file) => {
    const name = file.name.toLowerCase()
    return (
      name.endsWith('.json') ||
      name.endsWith('.txt') ||
      file.type === 'application/json' ||
      file.type === 'text/plain' ||
      file.type === ''
    )
  })
}

function countPasteLines(text: string): number {
  const trimmed = text.trim()
  if (!trimmed) return 0
  if (trimmed.startsWith('{') || trimmed.startsWith('[')) return 1
  return trimmed
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line && !line.startsWith('#')).length
}

function pastePlaceholder(
  method: AuthMethod,
  t: (key: string) => string
): string {
  if (method === 'refresh_token' || method === 'mobile_refresh_token') {
    return t('One refresh token per line (batch supported)')
  }
  if (method === 'codex_pat') {
    return t('One Codex PAT (at-...) per line (batch supported)')
  }
  return t(
    'Paste Codex session JSON, access tokens, or one credential per line'
  )
}

function pasteHelp(method: AuthMethod, t: (key: string) => string): string {
  if (method === 'refresh_token') {
    return t(
      'Manual RT: each line is exchanged via OpenAI OAuth and imported as a Codex channel.'
    )
  }
  if (method === 'mobile_refresh_token') {
    return t(
      'Mobile RT: uses the mobile OAuth client; paste one token per line.'
    )
  }
  if (method === 'codex_pat') {
    return t(
      'Codex Personal Access Token (at-*). Validated via whoami, then imported. One per line.'
    )
  }
  return t(
    'Codex JSON / AT: full session JSON, NDJSON, or access tokens (one per line).'
  )
}

export function ImportDataDialog(props: ImportDataDialogProps) {
  const { t } = useTranslation()
  const queryClient = useQueryClient()
  const fileRef = useRef<HTMLInputElement>(null)
  const [authMethod, setAuthMethod] = useState<AuthMethod>('file')
  const [mode, setMode] = useState<ImportMode>('auto')
  const [files, setFiles] = useState<File[]>([])
  const [pasteText, setPasteText] = useState('')
  const [group, setGroup] = useState('default')
  const [proxyId, setProxyId] = useState('__none__')
  const [isSubmitting, setIsSubmitting] = useState(false)
  const [isDragging, setIsDragging] = useState(false)

  const { data: groupsData } = useQuery({
    queryKey: ['groups'],
    queryFn: getGroups,
    staleTime: 5 * 60 * 1000,
    enabled: props.open,
  })

  const { data: proxyPoolData } = useQuery({
    queryKey: ['proxies'],
    queryFn: listProxies,
    staleTime: 5 * 60 * 1000,
    enabled: props.open,
  })

  const groupOptions = useMemo(() => {
    const names = new Set<string>(['default'])
    for (const name of groupsData?.data ?? []) {
      if (name) names.add(name)
    }
    if (group) names.add(group)
    return [...names]
  }, [groupsData, group])

  const enabledProxies = useMemo(
    () =>
      (proxyPoolData?.data ?? []).filter(
        (proxy: Proxy) => proxy.status === PROXY_STATUS.ENABLED
      ),
    [proxyPoolData]
  )

  const pasteCount = countPasteLines(pasteText)
  const canSubmit =
    authMethod === 'file' ? files.length > 0 : pasteText.trim().length > 0

  const reset = () => {
    setFiles([])
    setPasteText('')
    setGroup('default')
    setProxyId('__none__')
    setIsDragging(false)
    setAuthMethod('file')
    setMode('auto')
    if (fileRef.current) fileRef.current.value = ''
  }

  const handleFiles = (list: FileList | File[] | null) => {
    const next = collectJsonFiles(list)
    if (!next.length) {
      toast.error(t('Please drop or choose JSON auth files'))
      return
    }
    setFiles(next)
  }

  const summarize = (data: {
    format?: string
    channels?: {
      created?: number
      updated?: number
      skipped?: number
      failed?: number
    }
    data?: {
      proxy_created?: number
      proxy_reused?: number
      account_created?: number
      account_failed?: number
      proxy_failed?: number
    }
    file_results?: Array<{ ok: boolean; name: string; message?: string }>
  }) => {
    if (data.data) {
      const d = data.data
      toast.success(
        t(
          'Import done: proxies +{{createdP}}/reuse {{reusedP}}, accounts +{{createdA}}, failed P{{failedP}}/A{{failedA}}',
          {
            createdP: d.proxy_created ?? 0,
            reusedP: d.proxy_reused ?? 0,
            createdA: d.account_created ?? 0,
            failedP: d.proxy_failed ?? 0,
            failedA: d.account_failed ?? 0,
          }
        )
      )
      return
    }
    const c = data.channels
    if (c) {
      toast.success(
        t(
          'Auth import ({{format}}): created {{created}}, updated {{updated}}, skipped {{skipped}}, failed {{failed}}',
          {
            format: data.format || 'auto',
            created: c.created ?? 0,
            updated: c.updated ?? 0,
            skipped: c.skipped ?? 0,
            failed: c.failed ?? 0,
          }
        )
      )
    }
    const bad = data.file_results?.filter((f) => !f.ok) ?? []
    if (bad.length) {
      toast.message(t('{{count}} file(s) had errors', { count: bad.length }))
    }
  }

  const handleImport = async () => {
    setIsSubmitting(true)
    try {
      const defaultGroup = group.trim() || 'default'
      const selectedProxyId =
        proxyId === '__none__' ? undefined : Number(proxyId)

      if (authMethod === 'file') {
        if (!files.length) {
          toast.error(t('Please select a data file'))
          return
        }
        const format = mode === 'auto' ? 'auto' : mode
        const useMultipart = files.length > 1 || mode === 'cliproxy'
        const result = useMultipart
          ? await importAuthUpload({
              files,
              format,
              auth_method: 'auto',
              group: defaultGroup,
              proxy_id: selectedProxyId,
              update_existing: true,
            })
          : await importAuthJson({
              format,
              auth_method: 'auto',
              content: await files[0].text(),
              filenames: [files[0].name],
              group: defaultGroup,
              proxy_id: selectedProxyId,
              update_existing: true,
            })

        if (!result.success || !result.data) {
          toast.error(result.message || t('Import failed'))
          return
        }
        summarize(result.data)
      } else {
        const content = pasteText.trim()
        if (!content) {
          toast.error(t('Please paste credentials'))
          return
        }
        // Send the explicit method so raw AT, RT, mobile RT, and PAT cannot be confused.
        let pasteFilename = 'paste-refresh-token.txt'
        if (authMethod === 'codex_pat') {
          pasteFilename = 'paste-pat.txt'
        } else if (authMethod === 'codex_session') {
          pasteFilename = 'paste-session.txt'
        }
        const result = await importAuthJson({
          format: 'codex-session',
          auth_method: apiAuthMethod(authMethod),
          content,
          filenames: [pasteFilename],
          group: defaultGroup,
          proxy_id: selectedProxyId,
          update_existing: true,
        })
        if (!result.success || !result.data) {
          toast.error(result.message || t('Import failed'))
          return
        }
        summarize(result.data)
      }

      await queryClient.invalidateQueries({
        queryKey: channelsQueryKeys.lists(),
      })
      props.onOpenChange(false)
      reset()
    } catch (err) {
      toast.error(err instanceof Error ? err.message : t('Import failed'))
    } finally {
      setIsSubmitting(false)
    }
  }

  const authMethods: Array<{ value: AuthMethod; label: string }> = [
    { value: 'file', label: t('File upload') },
    { value: 'refresh_token', label: t('Manual RT') },
    { value: 'mobile_refresh_token', label: t('Mobile RT') },
    { value: 'codex_session', label: t('Codex JSON / AT') },
    { value: 'codex_pat', label: t('Codex Personal Access Token') },
  ]

  return (
    <Dialog
      open={props.open}
      onOpenChange={(open) => {
        props.onOpenChange(open)
        if (!open) reset()
      }}
    >
      <DialogContent className='sm:max-w-xl'>
        <DialogHeader>
          <DialogTitle>{t('Import credentials')}</DialogTitle>
          <DialogDescription>
            {t(
              'OpenAI / Codex authorization: manual RT, mobile RT, Codex JSON/AT, PAT, multi-file upload, and API batch paste. Groups are not auto-bound — set a default group below if needed.'
            )}
          </DialogDescription>
        </DialogHeader>

        <div className='space-y-4'>
          <div className='space-y-2'>
            <Label className='flex items-center gap-1.5'>
              <KeyRound className='size-3.5' />
              {t('Authorization Method')}
            </Label>
            <div className='flex flex-wrap gap-1.5'>
              {authMethods.map((item) => (
                <Button
                  key={item.value}
                  type='button'
                  size='sm'
                  variant={authMethod === item.value ? 'default' : 'outline'}
                  className='h-8'
                  onClick={() => setAuthMethod(item.value)}
                >
                  {item.label}
                </Button>
              ))}
            </div>
          </div>

          {authMethod === 'file' ? (
            <>
              <div className='space-y-2'>
                <Label>{t('Format')}</Label>
                <div className='flex flex-wrap gap-2'>
                  {(
                    [
                      ['auto', t('Auto-detect')],
                      ['sub2api-data', t('Sub2API data JSON')],
                      ['cliproxy', t('CLIProxyAPI auth')],
                      ['codex-session', t('Codex session')],
                    ] as const
                  ).map(([value, label]) => (
                    <Button
                      key={value}
                      type='button'
                      size='sm'
                      variant={mode === value ? 'default' : 'outline'}
                      onClick={() => setMode(value)}
                    >
                      {label}
                    </Button>
                  ))}
                </div>
              </div>

              <div className='space-y-2'>
                <Label>{t('Data file')}</Label>
                <p className='text-muted-foreground text-xs'>
                  {fileHelpText(mode, t)}
                </p>
                <div
                  role='button'
                  tabIndex={0}
                  className={cn(
                    'border-muted-foreground/30 bg-muted/20 hover:bg-muted/30 focus-visible:ring-ring rounded-xl border border-dashed px-4 py-6 text-center transition-[background-color,border-color] duration-[var(--duration-fast)] ease-[var(--ease-out)] outline-none focus-visible:ring-2',
                    isDragging && 'border-primary bg-primary/5'
                  )}
                  onClick={() => fileRef.current?.click()}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' || e.key === ' ') {
                      e.preventDefault()
                      fileRef.current?.click()
                    }
                  }}
                  onDragEnter={(e) => {
                    e.preventDefault()
                    e.stopPropagation()
                    setIsDragging(true)
                  }}
                  onDragOver={(e) => {
                    e.preventDefault()
                    e.stopPropagation()
                    setIsDragging(true)
                  }}
                  onDragLeave={(e) => {
                    e.preventDefault()
                    e.stopPropagation()
                    setIsDragging(false)
                  }}
                  onDrop={(e) => {
                    e.preventDefault()
                    e.stopPropagation()
                    setIsDragging(false)
                    handleFiles(e.dataTransfer.files)
                  }}
                >
                  <Upload className='text-muted-foreground mx-auto mb-2 h-6 w-6' />
                  <p className='text-sm font-medium'>
                    {t('Drop JSON files here, or click to choose')}
                  </p>
                  <p className='text-muted-foreground mt-1 text-xs'>
                    {t('Supports multi-file CLIProxyAPI auth upload')}
                  </p>
                  <div className='mt-3 flex items-center justify-center gap-2'>
                    <Button
                      type='button'
                      variant='outline'
                      size='sm'
                      onClick={(e) => {
                        e.stopPropagation()
                        fileRef.current?.click()
                      }}
                    >
                      <Upload className='mr-2 h-4 w-4' />
                      {t('Choose file')}
                    </Button>
                    <span className='text-muted-foreground truncate text-sm'>
                      {(() => {
                        if (files.length === 0) return t('No file selected')
                        if (files.length === 1) return files[0].name
                        return t('{{count}} files selected', {
                          count: files.length,
                        })
                      })()}
                    </span>
                  </div>
                  <input
                    ref={fileRef}
                    type='file'
                    multiple
                    accept='.json,.txt,application/json,text/plain,*/*'
                    className='hidden'
                    onChange={(e) => {
                      handleFiles(e.target.files)
                    }}
                  />
                </div>
                {files.length > 0 ? (
                  <div className='bg-muted/40 max-h-28 space-y-1 overflow-auto rounded-md border px-3 py-2 text-xs'>
                    {files.map((f) => (
                      <div
                        key={`${f.name}-${f.size}-${f.lastModified}`}
                        className='flex items-center gap-2'
                      >
                        <FileJson className='h-3.5 w-3.5 shrink-0' />
                        <span className='truncate'>
                          {f.name} ({f.size} B)
                        </span>
                      </div>
                    ))}
                  </div>
                ) : null}
              </div>
            </>
          ) : (
            <div className='space-y-2'>
              <div className='flex items-center justify-between gap-2'>
                <Label htmlFor='import-paste'>{t('Credentials')}</Label>
                {pasteCount > 1 ? (
                  <span className='bg-primary text-primary-foreground rounded-full px-2 py-0.5 text-[10px] font-medium'>
                    {t('{{count}} items', { count: pasteCount })}
                  </span>
                ) : null}
              </div>
              <p className='text-muted-foreground text-xs'>
                {pasteHelp(authMethod, t)}
              </p>
              <Textarea
                id='import-paste'
                value={pasteText}
                onChange={(e) => setPasteText(e.target.value)}
                placeholder={pastePlaceholder(authMethod, t)}
                className='min-h-36 font-mono text-xs'
                spellCheck={false}
              />
              {pasteCount > 1 ? (
                <p className='text-muted-foreground text-xs'>
                  {t('Batch create {{count}} channels', { count: pasteCount })}
                </p>
              ) : null}
            </div>
          )}

          <div className='space-y-2'>
            <Label htmlFor='import-group'>{t('Default group')}</Label>
            <Select
              items={groupOptions.map((name) => ({
                value: name,
                label: name,
              }))}
              value={group}
              onValueChange={(value) => {
                if (value) setGroup(value)
              }}
            >
              <SelectTrigger id='import-group' className='w-full'>
                <SelectValue placeholder={t('Select a group')} />
              </SelectTrigger>
              <SelectContent alignItemWithTrigger={false}>
                <SelectGroup>
                  {groupOptions.map((name) => (
                    <SelectItem key={name} value={name}>
                      {name}
                    </SelectItem>
                  ))}
                </SelectGroup>
              </SelectContent>
            </Select>
            <p className='text-muted-foreground text-xs'>
              {t(
                'Applied to new channels. Rebind groups in channel settings if needed.'
              )}
            </p>
          </div>

          <div className='space-y-2'>
            <Label htmlFor='import-proxy'>{t('Proxy Address')}</Label>
            <Select
              items={[
                { value: '__none__', label: t('None') },
                ...enabledProxies.map((proxy) => ({
                  value: String(proxy.id),
                  label: `${proxy.name} (${proxy.url})`,
                })),
              ]}
              value={proxyId}
              onValueChange={(value) => {
                if (value) setProxyId(value)
              }}
            >
              <SelectTrigger id='import-proxy' className='w-full'>
                <SelectValue placeholder={t('None')} />
              </SelectTrigger>
              <SelectContent alignItemWithTrigger={false}>
                <SelectGroup>
                  <SelectItem value='__none__'>{t('None')}</SelectItem>
                  {enabledProxies.map((proxy) => (
                    <SelectItem key={proxy.id} value={String(proxy.id)}>
                      {proxy.name}
                      <span className='text-muted-foreground ml-2 font-mono text-xs'>
                        {proxy.url}
                      </span>
                    </SelectItem>
                  ))}
                </SelectGroup>
              </SelectContent>
            </Select>
            <p className='text-muted-foreground text-xs'>
              {t('Network proxy for this channel (supports socks5 protocol)')}
            </p>
          </div>
        </div>

        <DialogFooter>
          <Button
            type='button'
            variant='outline'
            onClick={() => props.onOpenChange(false)}
            disabled={isSubmitting}
          >
            {t('Cancel')}
          </Button>
          <Button
            type='button'
            onClick={() => {
              void handleImport()
            }}
            disabled={isSubmitting || !canSubmit}
          >
            {isSubmitting ? t('Importing...') : t('Import')}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

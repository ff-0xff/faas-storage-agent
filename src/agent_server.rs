/*
 * @Author: why
 * @Date: 2021-07-31 16:58:16
 * @LastEditTime: 2021-08-13 14:52:05
 * @LastEditors: why
 * @Description: 
 * @FilePath: /sa/agent_server/src/agent_server.rs
 * 
 */

mod storage_ns;
mod redis_sa;
mod faas_storage_agent;
mod faas_storage_agent_grpc;
use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;
use std::time::SystemTime;
use std::{io, thread};
use faas_storage_agent::*;
use faas_storage_agent_grpc::*;
use futures::channel::oneshot;
use futures::executor::block_on;
use futures::prelude::*;
use grpcio::{Environment, Error, RpcContext, ServerBuilder, UnarySink};
use storage_ns::Namespace;
use crate::storage_ns::Backend;

#[derive(PartialEq,Clone,Debug)]
struct ClientInfo{
    token: String,
    client_id: String,
    //Unix timestamp
    lifetime: u64,
    scope: Vec<String>,
    current_ns: Namespace
}

#[derive(Default,Clone)]
struct AuthenticationInfo{
    client_id: String,
    lifetime: u64,
    //TODO: Init this field and check it in operations.
    scope: Vec<String>
}

#[derive(PartialEq,Clone,Default,Debug)]
struct AgentService{
    backend_name: String,
    client_cache: HashMap<String, ClientInfo>
}

impl AgentService {
    fn set_backend_name(&mut self, name: String) {
        self.backend_name = name;
    }

    fn cache_client_info(&mut self, token: &str, auth_info: AuthenticationInfo, ns: &Namespace) {
        self.client_cache.insert(token.to_string(), ClientInfo{
            token: token.to_string(),
            client_id: auth_info.client_id,
            lifetime: auth_info.lifetime,
            scope: auth_info.scope,
            current_ns: ns.clone(),
        });
        println!("after cache : {:?}", self.client_cache.get(&token.to_string()));
    }

    fn update_client_cache(&mut self,mut client_info: ClientInfo, new_ns_name: &str) ->Result<ClientInfo, bool> {
        let md = redis_sa::Metadata::default();
        let mut nsm = storage_ns::NsManager::new(&md);
        if let Ok(new_ns) = nsm.get_backend_ns(client_info.client_id.as_str(),  new_ns_name){
            client_info.current_ns = new_ns;
            self.client_cache.remove(&client_info.token);
            self.client_cache.insert(client_info.token.clone(), client_info.clone());
            Ok(client_info)
        }
        else {
            Err(false)
        }
    }

    fn check_get_client_cache(&mut self, token: &str, ns_name: &str) -> Result<ClientInfo, bool> {
        if let Ok(client_info) = self.is_client_alive(token){
            if client_info.current_ns.name != *ns_name {
                //Replace the current namespace with new namespace
                
                Ok(self.update_client_cache(client_info, ns_name)?)
            }
            else {
                Ok(client_info)
            }
        }
        else {
            Err(false)
        }
    }
    
    fn get_client_cache(&mut self, token: &str) -> Result<ClientInfo, bool> {
        if let Ok(client_info) = self.is_client_alive(token){
            println!("get cache successfully");
            return Ok(client_info)
        }
        println!("get cache failed");
        Err(false)
    }

    fn is_client_alive(&mut self, token: &str) -> Result<ClientInfo, bool> {
        if let Some(client) = self.client_cache.get(&token.to_string()){
            println!("is client alive = {:?}", client);
            if let Ok(dur) = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH){
                let now = dur.as_secs();
                println!("now : {}  lifetime {}", now, client.lifetime);
                if now < client.lifetime {
                    return Ok(client.to_owned())
                }
                else {
                    self.client_cache.remove(token);
                }
            }
        }
        Err(false)
    }

}

fn new_err_ns_resp(err_code: i32, err_info: &str) ->ns_resp {
    let mut resp = ns_resp::new();
    resp.set_err_code(err_code);
    resp.set_err_info(err_info.to_string());
    resp
}

fn new_err_data_resp(err_code: i32, err_info: &str) ->data_resp {
    let mut resp = data_resp::new();
    resp.set_err_code(err_code);
    resp.set_err_info(err_info.to_string());
    resp
}

impl FaasStorageAgent for AgentService {

    fn connect_ns(&mut self, ctx:RpcContext<'_>, req: ns_req, sink: UnarySink<ns_resp>){
        println!("connect_ns = {:?}", req);
        let resp: ns_resp;
        //Hit cache
        if let Ok(client_info) = self.check_get_client_cache(req.get_token(), req.get_name()) {
            println!("Hit cache {:?}", client_info);
            match self.backend_name.as_str() {
                "Redis" => resp = redis_sa::connect_ns(client_info.current_ns),
                _ => resp = new_err_ns_resp(4, "Error backen_name"),
            }
        }
        //Cache miss
        else {
            //Validate token successfully
            if let Ok(auth_info) = validate_token(req.get_token()) {
                let md = redis_sa::Metadata::default();
                let mut nsm = storage_ns::NsManager::new(&md);
                //Cache ns info
                if let Ok(ns) = nsm.get_backend_ns(auth_info.client_id.as_str(),  req.get_name()){
                    self.cache_client_info(req.get_token(), auth_info, &ns);
                    resp = redis_sa::connect_ns(ns)
                }
                else {
                    resp = new_err_ns_resp(1, "can not find this namespace")
                }
            }
            else {
                resp = new_err_ns_resp(2, "Client validation failed")
            }
        }
        let f = sink
        .success(resp)
        .map_err(move |e: Error| println!("failed to reply {:?}: {:?}", req, e))
        .map(|_| ());
        ctx.spawn(f)
    }

    fn create_ns(&mut self, ctx:RpcContext<'_>, req: ns_req, sink: UnarySink<ns_resp>){
        println!("create_ns = {:?}", req);
        let mut resp = ns_resp::default();
        let backend = Backend::Redis(redis_sa::Metadata::default());
        let md = redis_sa::Metadata::default();
        let mut nsm = storage_ns::NsManager::new(&md);
        //Hit cache
        if let Ok(client_info) = self.check_get_client_cache(req.get_token(), req.get_name()) {
            println!("Hit cache {:?}", client_info);
            match self.backend_name.as_str() {
                "Redis" => {
                    if nsm.new_backend_ns(client_info.client_id.as_str(),  req.get_name(), backend){
                        resp.set_err_code(0);
                        resp.set_err_info("Create ns successfully".to_string());
                    }
                    else {
                        resp = new_err_ns_resp(3, "can not create new namespace")
                    }
                },
                _ => resp = new_err_ns_resp(4, "Error backen_name"),
            }
        }
        //Cache miss
        else {
            //Validate token successfully
            if let Ok(auth_info) = validate_token(req.get_token()) {
                if nsm.new_backend_ns(auth_info.client_id.as_str(),  req.get_name(), backend){
                    resp.set_err_code(0);
                    resp.set_err_info("Create ns succ".to_string());
                }
                else {
                    resp = new_err_ns_resp(3, "can not create this namespace")
                }
            }
            else {
                resp = new_err_ns_resp(2, "Client validation failed")
            }
        }
        let f = sink
        .success(resp)
        .map_err(move |e: Error| println!("failed to reply {:?}: {:?}", req, e))
        .map(|_| ());
        ctx.spawn(f)
    }
    
    fn delete_ns(&mut self, ctx:RpcContext<'_>, req: ns_req, sink: UnarySink<ns_resp>){
        println!("delete_ns = {:?}", req);
        let mut resp = ns_resp::default();
        let md = redis_sa::Metadata::default();
        let mut nsm = storage_ns::NsManager::new(&md);
        //Hit cache
        if let Ok(client_info) = self.check_get_client_cache(req.get_token(), req.get_name()) {
            println!("Hit cache {:?}", client_info);
            nsm.delete_backend_ns(&client_info.current_ns);
        }
        //Cache miss
        else {
            //Validate token successfully
            if let Ok(auth_info) = validate_token(req.get_token()) {
                //Cache ns info
                if let Ok(ns) = nsm.get_backend_ns(auth_info.client_id.as_str(),  req.get_name()){
                    self.cache_client_info(req.get_token(), auth_info.clone(), &ns);
                    if nsm.delete_backend_ns(&ns) {
                        resp.set_err_code(0);
                    }
                    else {
                        resp = new_err_ns_resp(3, "can delete this namespace")
                    }
                }
                else {
                    resp = new_err_ns_resp(1, "can not find this namespace")
                }
            }
            else {
                resp = new_err_ns_resp(2, "Client validation failed")
            }
        }
        let f = sink
        .success(resp)
        .map_err(move |e: Error| println!("failed to reply {:?}: {:?}", req, e))
        .map(|_| ());
        ctx.spawn(f)
    }

    fn set(&mut self, ctx:RpcContext<'_>, req: data_req, sink: UnarySink<data_resp>){
        println!("set = {:?}", req);
        let resp: data_resp;
        //Hit cache
        if let Ok(client_info) = self.get_client_cache(req.get_token()) {
            println!("Hit cache {:?}", client_info);
            match self.backend_name.as_str() {
                "Redis" => resp = redis_sa::set(&req, client_info.current_ns),
                _ => resp = new_err_data_resp(4, "Error backen_name"),
            }
        }
        //Cache miss
        else {
            resp = new_err_data_resp(2, "Connect to the namespace first.");
        }
        let f = sink
            .success(resp)
            .map_err(move |e: Error| println!("failed to reply {:?}: {:?}", req, e))
            .map(|_| ());
        ctx.spawn(f)
    }

    fn get(&mut self, ctx:RpcContext<'_>, req: data_req, sink: UnarySink<data_resp>){
        println!("get = {:?}", req);
        let resp: data_resp;
        //Hit cache
        if let Ok(client_info) = self.get_client_cache(req.get_token()) {
            match self.backend_name.as_str() {
                "Redis" => resp = redis_sa::get(&req, client_info.current_ns),
                _ => resp = new_err_data_resp(4, "Error backen_name"),
            }
        }
        //Cache miss
        else {
            resp = new_err_data_resp(2, "Connect to the namespace first.");
        }
        let f = sink
            .success(resp)
            .map_err(move |e: Error| println!("failed to reply {:?}: {:?}", req, e))
            .map(|_| ());
        ctx.spawn(f)
    }

    fn delete(&mut self, ctx:RpcContext<'_>, req: data_req, sink: UnarySink<data_resp>){
        println!("delete = {:?}", req);
        let resp: data_resp;
        //Hit cache
        if let Ok(client_info) = self.get_client_cache(req.get_token()) {
            match self.backend_name.as_str() {
                "Redis" => resp = redis_sa::delete(&req, client_info.current_ns),
                _ => resp = new_err_data_resp(4, "Error backen_name"),
            }
        }
        //Cache miss
        else {
            resp = new_err_data_resp(2, "Connect to the namespace first.");
        }
        let f = sink
            .success(resp)
            .map_err(move |e: Error| println!("failed to reply {:?}: {:?}", req, e))
            .map(|_| ());
        ctx.spawn(f)
    }

    fn exists(&mut self, ctx:RpcContext<'_>, req: data_req, sink: UnarySink<data_resp>){
        println!("exists = {:?}", req);
        let resp: data_resp;
        //Hit cache
        if let Ok(client_info) = self.get_client_cache(req.get_token()) {
            match self.backend_name.as_str() {
                "Redis" => resp = redis_sa::exists(&req, client_info.current_ns),
                _ => resp = new_err_data_resp(4, "Error backen_name"),
            }
        }
        //Cache miss
        else {
            resp = new_err_data_resp(2, "Connect to the namespace first.");
        }
        let f = sink
            .success(resp)
            .map_err(move |e: Error| println!("failed to reply {:?}: {:?}", req, e))
            .map(|_| ());
        ctx.spawn(f)
    }

}

fn main() {
    println!("Hello, world!");
    let mut agent_service = AgentService::default();
    agent_service.set_backend_name("Redis".to_string());
    let svc = create_faas_storage_agent(agent_service);
    let env = Arc::new(Environment::new(1));
    let mut server = ServerBuilder::new(env)
        .register_service(svc)
        .bind("127.0.0.1", 10086)
        .build()
        .unwrap();
    server.start();
    for (host, port) in server.bind_addrs() {
        println!("listening on {}:{}", host, port);
    }
    let (tx, rx) = oneshot::channel();
    thread::spawn(move || {
        println!("Press ENTER to exit...");
        let _ = io::stdin().read(&mut [0]).unwrap();
        tx.send(())
    });
    let _ = block_on(rx);
    let _ = block_on(server.shutdown());
}


fn validate_token(token: &str) -> Result<AuthenticationInfo, bool> {
    let client_id = env!("sas_client_id");
    let client_secret = env!("sas_client_secret");
    let credential= format!("{}:{}", client_id, client_secret);
    //println!("{}",credential);
    let credential_base64 = base64::encode(credential.as_bytes());
    let auth_content = "Basic ".to_string() + &credential_base64;
    let body = "token:".to_string() + token;
    // let req_client = reqwest::blocking::Client::new();
    // let res = req_client.post("http://127.0.0.1:10087/o/introspect/")
    //     .body(body)
    //     .header("Authorization ", auth_content)
    //     .send()
    //     .expect("Send Err");
    // println!("res : {:?}", res);
    //TODO: Replace the tmp rv
    Ok(AuthenticationInfo{
        client_id: "test".to_string(),
        lifetime: 1728392084,
        scope: vec!["test".to_string()]
    })
}